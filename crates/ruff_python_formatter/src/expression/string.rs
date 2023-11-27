use std::borrow::Cow;

use bitflags::bitflags;

use ruff_formatter::{format_args, write, Printed};
use ruff_python_ast::AnyNodeRef;
use ruff_python_ast::{
    self as ast, ExprBytesLiteral, ExprFString, ExprStringLiteral, ExpressionRef,
};
use ruff_source_file::Locator;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::comments::{leading_comments, trailing_comments};
use crate::expression::parentheses::{
    in_parentheses_only_group, in_parentheses_only_soft_line_break_or_space,
};
use crate::expression::Expr;
use crate::prelude::*;
use crate::FormatModuleError;
use crate::QuoteStyle;

#[derive(Copy, Clone, Debug)]
enum Quoting {
    CanChange,
    Preserve,
}

#[derive(Clone, Debug)]
pub(super) enum AnyString<'a> {
    String(&'a ExprStringLiteral),
    Bytes(&'a ExprBytesLiteral),
    FString(&'a ExprFString),
}

impl<'a> AnyString<'a> {
    pub(crate) fn from_expression(expression: &'a Expr) -> Option<AnyString<'a>> {
        match expression {
            Expr::StringLiteral(string) => Some(AnyString::String(string)),
            Expr::BytesLiteral(bytes) => Some(AnyString::Bytes(bytes)),
            Expr::FString(fstring) => Some(AnyString::FString(fstring)),
            _ => None,
        }
    }

    fn quoting(&self, locator: &Locator) -> Quoting {
        match self {
            Self::String(_) | Self::Bytes(_) => Quoting::CanChange,
            Self::FString(f_string) => {
                let unprefixed = locator
                    .slice(f_string.range)
                    .trim_start_matches(|c| c != '"' && c != '\'');
                let triple_quoted =
                    unprefixed.starts_with(r#"""""#) || unprefixed.starts_with(r"'''");
                if f_string.value.elements().any(|value| match value {
                    Expr::FormattedValue(ast::ExprFormattedValue { range, .. }) => {
                        let string_content = locator.slice(*range);
                        if triple_quoted {
                            string_content.contains(r#"""""#) || string_content.contains("'''")
                        } else {
                            string_content.contains(['"', '\''])
                        }
                    }
                    _ => false,
                }) {
                    Quoting::Preserve
                } else {
                    Quoting::CanChange
                }
            }
        }
    }

    /// Returns `true` if the string is implicitly concatenated.
    pub(super) fn is_implicit_concatenated(&self) -> bool {
        match self {
            Self::String(ExprStringLiteral { value, .. }) => value.is_implicit_concatenated(),
            Self::Bytes(ExprBytesLiteral { value, .. }) => value.is_implicit_concatenated(),
            Self::FString(ExprFString { value, .. }) => value.is_implicit_concatenated(),
        }
    }

    fn parts(&self) -> Vec<AnyStringPart<'a>> {
        match self {
            Self::String(ExprStringLiteral { value, .. }) => {
                value.parts().map(AnyStringPart::String).collect()
            }
            Self::Bytes(ExprBytesLiteral { value, .. }) => {
                value.parts().map(AnyStringPart::Bytes).collect()
            }
            Self::FString(ExprFString { value, .. }) => value
                .parts()
                .map(|f_string_part| match f_string_part {
                    ast::FStringPart::Literal(string_literal) => {
                        AnyStringPart::String(string_literal)
                    }
                    ast::FStringPart::FString(f_string) => AnyStringPart::FString(f_string),
                })
                .collect(),
        }
    }
}

impl Ranged for AnyString<'_> {
    fn range(&self) -> TextRange {
        match self {
            Self::String(expr) => expr.range(),
            Self::Bytes(expr) => expr.range(),
            Self::FString(expr) => expr.range(),
        }
    }
}

impl<'a> From<&AnyString<'a>> for AnyNodeRef<'a> {
    fn from(value: &AnyString<'a>) -> Self {
        match value {
            AnyString::String(expr) => AnyNodeRef::ExprStringLiteral(expr),
            AnyString::Bytes(expr) => AnyNodeRef::ExprBytesLiteral(expr),
            AnyString::FString(expr) => AnyNodeRef::ExprFString(expr),
        }
    }
}

impl<'a> From<&AnyString<'a>> for ExpressionRef<'a> {
    fn from(value: &AnyString<'a>) -> Self {
        match value {
            AnyString::String(expr) => ExpressionRef::StringLiteral(expr),
            AnyString::Bytes(expr) => ExpressionRef::BytesLiteral(expr),
            AnyString::FString(expr) => ExpressionRef::FString(expr),
        }
    }
}

#[derive(Clone, Debug)]
enum AnyStringPart<'a> {
    String(&'a ast::StringLiteral),
    Bytes(&'a ast::BytesLiteral),
    FString(&'a ast::FString),
}

impl<'a> From<&AnyStringPart<'a>> for AnyNodeRef<'a> {
    fn from(value: &AnyStringPart<'a>) -> Self {
        match value {
            AnyStringPart::String(part) => AnyNodeRef::StringLiteral(part),
            AnyStringPart::Bytes(part) => AnyNodeRef::BytesLiteral(part),
            AnyStringPart::FString(part) => AnyNodeRef::FString(part),
        }
    }
}

impl Ranged for AnyStringPart<'_> {
    fn range(&self) -> TextRange {
        match self {
            Self::String(part) => part.range(),
            Self::Bytes(part) => part.range(),
            Self::FString(part) => part.range(),
        }
    }
}

pub(super) struct FormatString<'a> {
    string: &'a AnyString<'a>,
    layout: StringLayout,
}

#[derive(Default, Copy, Clone, Debug)]
pub enum StringLayout {
    #[default]
    Default,
    DocString,
    /// An implicit concatenated string in a binary like (e.g. `a + b` or `a < b`) expression.
    ///
    /// Formats the implicit concatenated string parts without the enclosing group because the group
    /// is added by the binary like formatting.
    ImplicitConcatenatedStringInBinaryLike,
}

impl<'a> FormatString<'a> {
    pub(super) fn new(string: &'a AnyString<'a>) -> Self {
        Self {
            string,
            layout: StringLayout::Default,
        }
    }

    pub(super) fn with_layout(mut self, layout: StringLayout) -> Self {
        self.layout = layout;
        self
    }
}

impl<'a> Format<PyFormatContext<'_>> for FormatString<'a> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        let parent_docstring_quote_style = f.context().docstring();
        let locator = f.context().locator();
        let result = match self.layout {
            StringLayout::Default => {
                if self.string.is_implicit_concatenated() {
                    in_parentheses_only_group(&FormatStringContinuation::new(self.string)).fmt(f)
                } else {
                    StringPart::from_source(self.string.range(), &locator)
                        .normalize(
                            self.string.quoting(&locator),
                            &locator,
                            f.options().quote_style(),
                            parent_docstring_quote_style,
                        )
                        .fmt(f)
                }
            }
            StringLayout::DocString => {
                let string_part = StringPart::from_source(self.string.range(), &locator);
                let normalized = string_part.normalize(
                    Quoting::CanChange,
                    &locator,
                    f.options().quote_style(),
                    parent_docstring_quote_style,
                );
                format_docstring(&normalized, f)
            }
            StringLayout::ImplicitConcatenatedStringInBinaryLike => {
                FormatStringContinuation::new(self.string).fmt(f)
            }
        };
        // TODO(dhruvmanila): With PEP 701, comments can be inside f-strings.
        // This is to mark all of those comments as formatted but we need to
        // figure out how to handle them. Note that this needs to be done only
        // after the f-string is formatted, so only for all the non-formatted
        // comments.
        if let AnyString::FString(fstring) = self.string {
            let comments = f.context().comments();
            fstring.value.elements().for_each(|value| {
                comments.mark_verbatim_node_comments_formatted(value.into());
            });
        }
        result
    }
}

struct FormatStringContinuation<'a> {
    string: &'a AnyString<'a>,
}

impl<'a> FormatStringContinuation<'a> {
    fn new(string: &'a AnyString<'a>) -> Self {
        Self { string }
    }
}

impl Format<PyFormatContext<'_>> for FormatStringContinuation<'_> {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        let comments = f.context().comments().clone();
        let locator = f.context().locator();
        let in_docstring = f.context().docstring();
        let quote_style = f.options().quote_style();

        let mut joiner = f.join_with(in_parentheses_only_soft_line_break_or_space());

        for part in self.string.parts() {
            let normalized = StringPart::from_source(part.range(), &locator).normalize(
                self.string.quoting(&locator),
                &locator,
                quote_style,
                in_docstring,
            );

            joiner.entry(&format_args![
                line_suffix_boundary(),
                leading_comments(comments.leading(&part)),
                normalized,
                trailing_comments(comments.trailing(&part))
            ]);
        }

        joiner.finish()
    }
}

#[derive(Debug)]
struct StringPart {
    /// The prefix.
    prefix: StringPrefix,

    /// The actual quotes of the string in the source
    quotes: StringQuotes,

    /// The range of the string's content (full range minus quotes and prefix)
    content_range: TextRange,
}

impl StringPart {
    fn from_source(range: TextRange, locator: &Locator) -> Self {
        let string_content = locator.slice(range);

        let prefix = StringPrefix::parse(string_content);
        let after_prefix = &string_content[usize::from(prefix.text_len())..];

        let quotes =
            StringQuotes::parse(after_prefix).expect("Didn't find string quotes after prefix");
        let relative_raw_content_range = TextRange::new(
            prefix.text_len() + quotes.text_len(),
            string_content.text_len() - quotes.text_len(),
        );
        let raw_content_range = relative_raw_content_range + range.start();

        Self {
            prefix,
            content_range: raw_content_range,
            quotes,
        }
    }

    /// Computes the strings preferred quotes and normalizes its content.
    ///
    /// The parent docstring quote style should be set when formatting a code
    /// snippet within the docstring. The quote style should correspond to the
    /// style of quotes used by said docstring. Normalization will ensure the
    /// quoting styles don't conflict.
    fn normalize<'a>(
        self,
        quoting: Quoting,
        locator: &'a Locator,
        configured_style: QuoteStyle,
        parent_docstring_quote_style: Option<QuoteStyle>,
    ) -> NormalizedString<'a> {
        // Per PEP 8 and PEP 257, always prefer double quotes for docstrings
        // and triple-quoted strings. (We assume docstrings are always
        // triple-quoted.)
        let preferred_style = if self.quotes.triple {
            // ... unless we're formatting a code snippet inside a docstring,
            // then we specifically want to invert our quote style to avoid
            // writing out invalid Python.
            //
            // It's worth pointing out that we can actually wind up being
            // somewhat out of sync with PEP8 in this case. Consider this
            // example:
            //
            //     def foo():
            //         '''
            //         Something.
            //
            //         >>> """tricksy"""
            //         '''
            //         pass
            //
            // Ideally, this would be reformatted as:
            //
            //     def foo():
            //         """
            //         Something.
            //
            //         >>> '''tricksy'''
            //         """
            //         pass
            //
            // But the logic here results in the original quoting being
            // preserved. This is because the quoting style of the outer
            // docstring is determined, in part, by looking at its contents. In
            // this case, it notices that it contains a `"""` and thus infers
            // that using `'''` would overall read better because it avoids
            // the need to escape the interior `"""`. Except... in this case,
            // the `"""` is actually part of a code snippet that could get
            // reformatted to using a different quoting style itself.
            //
            // Fixing this would, I believe, require some fairly seismic
            // changes to how formatting strings works. Namely, we would need
            // to look for code snippets before normalizing the docstring, and
            // then figure out the quoting style more holistically by looking
            // at the various kinds of quotes used in the code snippets and
            // what reformatting them might look like.
            //
            // Overall this is a bit of a corner case and just inverting the
            // style from what the parent ultimately decided upon works, even
            // if it doesn't have perfect alignment with PEP8.
            if let Some(style) = parent_docstring_quote_style {
                style.invert()
            } else {
                QuoteStyle::Double
            }
        } else {
            configured_style
        };

        let raw_content = locator.slice(self.content_range);

        let quotes = match quoting {
            Quoting::Preserve => self.quotes,
            Quoting::CanChange => {
                if self.prefix.is_raw_string() {
                    choose_quotes_raw(raw_content, self.quotes, preferred_style)
                } else {
                    choose_quotes(raw_content, self.quotes, preferred_style)
                }
            }
        };

        let normalized = normalize_string(locator.slice(self.content_range), quotes, self.prefix);

        NormalizedString {
            prefix: self.prefix,
            content_range: self.content_range,
            text: normalized,
            quotes,
        }
    }
}

#[derive(Debug)]
struct NormalizedString<'a> {
    prefix: StringPrefix,

    /// The quotes of the normalized string (preferred quotes)
    quotes: StringQuotes,

    /// The range of the string's content in the source (minus prefix and quotes).
    content_range: TextRange,

    /// The normalized text
    text: Cow<'a, str>,
}

impl Ranged for NormalizedString<'_> {
    fn range(&self) -> TextRange {
        self.content_range
    }
}

impl Format<PyFormatContext<'_>> for NormalizedString<'_> {
    fn fmt(&self, f: &mut Formatter<PyFormatContext<'_>>) -> FormatResult<()> {
        write!(f, [self.prefix, self.quotes])?;
        match &self.text {
            Cow::Borrowed(_) => {
                source_text_slice(self.range()).fmt(f)?;
            }
            Cow::Owned(normalized) => {
                text(normalized, Some(self.start())).fmt(f)?;
            }
        }
        self.quotes.fmt(f)
    }
}

bitflags! {
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub(super) struct StringPrefix: u8 {
        const UNICODE   = 0b0000_0001;
        /// `r"test"`
        const RAW       = 0b0000_0010;
        /// `R"test"
        const RAW_UPPER = 0b0000_0100;
        const BYTE      = 0b0000_1000;
        const F_STRING  = 0b0001_0000;
    }
}

impl StringPrefix {
    pub(super) fn parse(input: &str) -> StringPrefix {
        let chars = input.chars();
        let mut prefix = StringPrefix::empty();

        for c in chars {
            let flag = match c {
                'u' | 'U' => StringPrefix::UNICODE,
                'f' | 'F' => StringPrefix::F_STRING,
                'b' | 'B' => StringPrefix::BYTE,
                'r' => StringPrefix::RAW,
                'R' => StringPrefix::RAW_UPPER,
                '\'' | '"' => break,
                c => {
                    unreachable!(
                        "Unexpected character '{c}' terminating the prefix of a string literal"
                    );
                }
            };

            prefix |= flag;
        }

        prefix
    }

    pub(super) const fn text_len(self) -> TextSize {
        TextSize::new(self.bits().count_ones())
    }

    pub(super) const fn is_raw_string(self) -> bool {
        self.contains(StringPrefix::RAW) || self.contains(StringPrefix::RAW_UPPER)
    }

    pub(super) const fn is_fstring(self) -> bool {
        self.contains(StringPrefix::F_STRING)
    }
}

impl Format<PyFormatContext<'_>> for StringPrefix {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        // Retain the casing for the raw prefix:
        // https://black.readthedocs.io/en/stable/the_black_code_style/current_style.html#r-strings-and-r-strings
        if self.contains(StringPrefix::RAW) {
            token("r").fmt(f)?;
        } else if self.contains(StringPrefix::RAW_UPPER) {
            token("R").fmt(f)?;
        }

        if self.contains(StringPrefix::BYTE) {
            token("b").fmt(f)?;
        }

        if self.contains(StringPrefix::F_STRING) {
            token("f").fmt(f)?;
        }

        // Remove the unicode prefix `u` if any because it is meaningless in Python 3+.

        Ok(())
    }
}

/// Choose the appropriate quote style for a raw string.
///
/// The preferred quote style is chosen unless the string contains unescaped quotes of the
/// preferred style. For example, `r"foo"` is chosen over `r'foo'` if the preferred quote
/// style is double quotes.
fn choose_quotes_raw(
    input: &str,
    quotes: StringQuotes,
    preferred_style: QuoteStyle,
) -> StringQuotes {
    let preferred_quote_char = preferred_style.as_char();
    let mut chars = input.chars().peekable();
    let contains_unescaped_configured_quotes = loop {
        match chars.next() {
            Some('\\') => {
                // Ignore escaped characters
                chars.next();
            }
            // `"` or `'`
            Some(c) if c == preferred_quote_char => {
                if !quotes.triple {
                    break true;
                }

                match chars.peek() {
                    // We can't turn `r'''\""'''` into `r"""\"""""`, this would confuse the parser
                    // about where the closing triple quotes start
                    None => break true,
                    Some(next) if *next == preferred_quote_char => {
                        // `""` or `''`
                        chars.next();

                        // We can't turn `r'''""'''` into `r""""""""`, nor can we have
                        // `"""` or `'''` respectively inside the string
                        if chars.peek().is_none() || chars.peek() == Some(&preferred_quote_char) {
                            break true;
                        }
                    }
                    _ => {}
                }
            }
            Some(_) => continue,
            None => break false,
        }
    };

    StringQuotes {
        triple: quotes.triple,
        style: if contains_unescaped_configured_quotes {
            quotes.style
        } else {
            preferred_style
        },
    }
}

/// Choose the appropriate quote style for a string.
///
/// For single quoted strings, the preferred quote style is used, unless the alternative quote style
/// would require fewer escapes.
///
/// For triple quoted strings, the preferred quote style is always used, unless the string contains
/// a triplet of the quote character (e.g., if double quotes are preferred, double quotes will be
/// used unless the string contains `"""`).
fn choose_quotes(input: &str, quotes: StringQuotes, preferred_style: QuoteStyle) -> StringQuotes {
    let style = if quotes.triple {
        // True if the string contains a triple quote sequence of the configured quote style.
        let mut uses_triple_quotes = false;
        let mut chars = input.chars().peekable();

        while let Some(c) = chars.next() {
            let preferred_quote_char = preferred_style.as_char();
            match c {
                '\\' => {
                    if matches!(chars.peek(), Some('"' | '\\')) {
                        chars.next();
                    }
                }
                // `"` or `'`
                c if c == preferred_quote_char => {
                    match chars.peek().copied() {
                        Some(c) if c == preferred_quote_char => {
                            // `""` or `''`
                            chars.next();

                            match chars.peek().copied() {
                                Some(c) if c == preferred_quote_char => {
                                    // `"""` or `'''`
                                    chars.next();
                                    uses_triple_quotes = true;
                                    break;
                                }
                                Some(_) => {}
                                None => {
                                    // Handle `''' ""'''`. At this point we have consumed both
                                    // double quotes, so on the next iteration the iterator is empty
                                    // and we'd miss the string ending with a preferred quote
                                    uses_triple_quotes = true;
                                    break;
                                }
                            }
                        }
                        Some(_) => {
                            // A single quote char, this is ok
                        }
                        None => {
                            // Trailing quote at the end of the comment
                            uses_triple_quotes = true;
                            break;
                        }
                    }
                }
                _ => continue,
            }
        }

        if uses_triple_quotes {
            // String contains a triple quote sequence of the configured quote style.
            // Keep the existing quote style.
            quotes.style
        } else {
            preferred_style
        }
    } else {
        let mut single_quotes = 0u32;
        let mut double_quotes = 0u32;

        for c in input.chars() {
            match c {
                '\'' => {
                    single_quotes += 1;
                }

                '"' => {
                    double_quotes += 1;
                }

                _ => continue,
            }
        }

        match preferred_style {
            QuoteStyle::Single => {
                if single_quotes > double_quotes {
                    QuoteStyle::Double
                } else {
                    QuoteStyle::Single
                }
            }
            QuoteStyle::Double => {
                if double_quotes > single_quotes {
                    QuoteStyle::Single
                } else {
                    QuoteStyle::Double
                }
            }
        }
    };

    StringQuotes {
        triple: quotes.triple,
        style,
    }
}

#[derive(Copy, Clone, Debug)]
pub(super) struct StringQuotes {
    triple: bool,
    style: QuoteStyle,
}

impl StringQuotes {
    pub(super) fn parse(input: &str) -> Option<StringQuotes> {
        let mut chars = input.chars();

        let quote_char = chars.next()?;
        let style = QuoteStyle::try_from(quote_char).ok()?;

        let triple = chars.next() == Some(quote_char) && chars.next() == Some(quote_char);

        Some(Self { triple, style })
    }

    pub(super) const fn is_triple(self) -> bool {
        self.triple
    }

    const fn text_len(self) -> TextSize {
        if self.triple {
            TextSize::new(3)
        } else {
            TextSize::new(1)
        }
    }
}

impl Format<PyFormatContext<'_>> for StringQuotes {
    fn fmt(&self, f: &mut PyFormatter) -> FormatResult<()> {
        let quotes = match (self.style, self.triple) {
            (QuoteStyle::Single, false) => "'",
            (QuoteStyle::Single, true) => "'''",
            (QuoteStyle::Double, false) => "\"",
            (QuoteStyle::Double, true) => "\"\"\"",
        };

        token(quotes).fmt(f)
    }
}

/// Adds the necessary quote escapes and removes unnecessary escape sequences when quoting `input`
/// with the provided [`StringQuotes`] style.
///
/// Returns the normalized string and whether it contains new lines.
fn normalize_string(input: &str, quotes: StringQuotes, prefix: StringPrefix) -> Cow<str> {
    // The normalized string if `input` is not yet normalized.
    // `output` must remain empty if `input` is already normalized.
    let mut output = String::new();
    // Tracks the last index of `input` that has been written to `output`.
    // If `last_index` is `0` at the end, then the input is already normalized and can be returned as is.
    let mut last_index = 0;

    let style = quotes.style;
    let preferred_quote = style.as_char();
    let opposite_quote = style.invert().as_char();

    let mut chars = input.char_indices().peekable();

    let is_raw = prefix.is_raw_string();
    let is_fstring = prefix.is_fstring();
    let mut formatted_value_nesting = 0u32;

    while let Some((index, c)) = chars.next() {
        if is_fstring && matches!(c, '{' | '}') {
            if chars.peek().copied().is_some_and(|(_, next)| next == c) {
                // Skip over the second character of the double braces
                chars.next();
            } else if c == '{' {
                formatted_value_nesting += 1;
            } else {
                // Safe to assume that `c == '}'` here because of the matched pattern above
                formatted_value_nesting = formatted_value_nesting.saturating_sub(1);
            }
            continue;
        }
        if c == '\r' {
            output.push_str(&input[last_index..index]);

            // Skip over the '\r' character, keep the `\n`
            if chars.peek().copied().is_some_and(|(_, next)| next == '\n') {
                chars.next();
            }
            // Replace the `\r` with a `\n`
            else {
                output.push('\n');
            }

            last_index = index + '\r'.len_utf8();
        } else if !quotes.triple && !is_raw {
            if c == '\\' {
                if let Some((_, next)) = chars.peek().copied() {
                    #[allow(clippy::if_same_then_else)]
                    if next == opposite_quote && formatted_value_nesting == 0 {
                        // Remove the escape by ending before the backslash and starting again with the quote
                        chars.next();
                        output.push_str(&input[last_index..index]);
                        last_index = index + '\\'.len_utf8();
                    } else if next == preferred_quote {
                        // Quote is already escaped, skip over it.
                        chars.next();
                    } else if next == '\\' {
                        // Skip over escaped backslashes
                        chars.next();
                    }
                }
            } else if c == preferred_quote && formatted_value_nesting == 0 {
                // Escape the quote
                output.push_str(&input[last_index..index]);
                output.push('\\');
                output.push(c);
                last_index = index + preferred_quote.len_utf8();
            }
        }
    }

    let normalized = if last_index == 0 {
        Cow::Borrowed(input)
    } else {
        output.push_str(&input[last_index..]);
        Cow::Owned(output)
    };

    normalized
}

/// For docstring indentation, black counts spaces as 1 and tabs by increasing the indentation up
/// to the next multiple of 8. This is effectively a port of
/// [`str.expandtabs`](https://docs.python.org/3/library/stdtypes.html#str.expandtabs),
/// which black [calls with the default tab width of 8](https://github.com/psf/black/blob/c36e468794f9256d5e922c399240d49782ba04f1/src/black/strings.py#L61).
fn indentation_length(line: &str) -> TextSize {
    let mut indentation = 0u32;
    for char in line.chars() {
        if char == '\t' {
            // Pad to the next multiple of tab_width
            indentation += 8 - (indentation.rem_euclid(8));
        } else if char.is_whitespace() {
            indentation += u32::from(char.text_len());
        } else {
            break;
        }
    }
    TextSize::new(indentation)
}

/// Format a docstring by trimming whitespace and adjusting the indentation.
///
/// Summary of changes we make:
/// * Normalize the string like all other strings
/// * Ignore docstring that have an escaped newline
/// * Trim all trailing whitespace, except for a chaperone space that avoids quotes or backslashes
///   in the last line.
/// * Trim leading whitespace on the first line, again except for a chaperone space
/// * If there is only content in the first line and after that only whitespace, collapse the
///   docstring into one line
/// * Adjust the indentation (see below)
///
/// # Docstring indentation
///
/// Unlike any other string, like black we change the indentation of docstring lines.
///
/// We want to preserve the indentation inside the docstring relative to the suite statement/block
/// indent that the docstring statement is in, but also want to apply the change of the outer
/// indentation in the docstring, e.g.
/// ```python
/// def sparkle_sky():
///   """Make a pretty sparkly sky.
///   *       * ✨        *.    .
///      *       *      ✨      .
///      .  *      . ✨    * .  .
///   """
/// ```
/// should become
/// ```python
/// def sparkle_sky():
///     """Make a pretty sparkly sky.
///     *       * ✨        *.    .
///        *       *      ✨      .
///        .  *      . ✨    * .  .
///     """
/// ```
/// We can't compute the full indentation here since we don't know what the block indent of
/// the doc comment will be yet and which we can only have added by formatting each line
/// separately with a hard line break. This means we need to strip shared indentation from
/// docstring while preserving the in-docstring bigger-than-suite-statement indentation. Example:
/// ```python
/// def f():
///  """first line
///  line a
///     line b
///  """
/// ```
/// The docstring indentation is 2, the block indents will change this to 4 (but we can't
/// determine this at this point). The indentation of line a is 2, so we trim `  line a`
/// to `line a`. For line b it's 5, so we trim it to `line b` and pad with 5-2=3 spaces to
/// `   line b`. The closing quotes, being on their own line, are stripped get only the
/// default indentation. Fully formatted:
/// ```python
/// def f():
///    """first line
///    line a
///       line b
///    """
/// ```
///
/// Tabs are counted by padding them to the next multiple of 8 according to
/// [`str.expandtabs`](https://docs.python.org/3/library/stdtypes.html#str.expandtabs). When
/// we see indentation that contains a tab or any other none ascii-space whitespace we rewrite the
/// string.
///
/// Additionally, if any line in the docstring has less indentation than the docstring
/// (effectively a negative indentation wrt. to the current level), we pad all lines to the
/// level of the docstring with spaces.
/// ```python
/// def f():
///    """first line
/// line a
///    line b
///      line c
///    """
/// ```
/// Here line a is 3 columns negatively indented, so we pad all lines by an extra 3 spaces:
/// ```python
/// def f():
///    """first line
///    line a
///       line b
///         line c
///    """
/// ```
fn format_docstring(normalized: &NormalizedString, f: &mut PyFormatter) -> FormatResult<()> {
    let docstring = &normalized.text;

    // Black doesn't change the indentation of docstrings that contain an escaped newline
    if docstring.contains("\\\n") {
        return normalized.fmt(f);
    }

    // is_borrowed is unstable :/
    let already_normalized = matches!(docstring, Cow::Borrowed(_));

    let mut lines = docstring.lines().peekable();

    // Start the string
    write!(
        f,
        [
            normalized.prefix,
            normalized.quotes,
            source_position(normalized.start()),
        ]
    )?;
    // We track where in the source docstring we are (in source code byte offsets)
    let mut offset = normalized.start();

    // The first line directly after the opening quotes has different rules than the rest, mainly
    // that we remove all leading whitespace as there's no indentation
    let first = lines.next().unwrap_or_default();
    // Black trims whitespace using [`str.strip()`](https://docs.python.org/3/library/stdtypes.html#str.strip)
    // https://github.com/psf/black/blob/b4dca26c7d93f930bbd5a7b552807370b60d4298/src/black/strings.py#L77-L85
    // So we use the unicode whitespace definition through `trim_{start,end}` instead of the python
    // tokenizer whitespace definition in `trim_whitespace_{start,end}`.
    let trim_end = first.trim_end();
    let trim_both = trim_end.trim_start();

    // Edge case: The first line is `""" "content`, so we need to insert chaperone space that keep
    // inner quotes and closing quotes from getting to close to avoid `""""content`
    if trim_both.starts_with(normalized.quotes.style.as_char()) {
        space().fmt(f)?;
    }

    if !trim_end.is_empty() {
        // For the first line of the docstring we strip the leading and trailing whitespace, e.g.
        // `"""   content   ` to `"""content`
        let leading_whitespace = trim_end.text_len() - trim_both.text_len();
        let trimmed_line_range =
            TextRange::at(offset, trim_end.text_len()).add_start(leading_whitespace);
        if already_normalized {
            source_text_slice(trimmed_line_range).fmt(f)?;
        } else {
            text(trim_both, Some(trimmed_line_range.start())).fmt(f)?;
        }
    }
    offset += first.text_len();

    // Check if we have a single line (or empty) docstring
    if docstring[first.len()..].trim().is_empty() {
        // For `"""\n"""` or other whitespace between the quotes, black keeps a single whitespace,
        // but `""""""` doesn't get one inserted.
        if needs_chaperone_space(normalized, trim_end)
            || (trim_end.is_empty() && !docstring.is_empty())
        {
            space().fmt(f)?;
        }
        normalized.quotes.fmt(f)?;
        return Ok(());
    }

    hard_line_break().fmt(f)?;
    // We know that the normalized string has \n line endings
    offset += "\n".text_len();

    // If some line of the docstring is less indented than the function body, we pad all lines to
    // align it with the docstring statement. Conversely, if all lines are over-indented, we strip
    // the extra indentation. We call this stripped indentation since it's relative to the block
    // indent printer-made indentation.
    let stripped_indentation_length = lines
        .clone()
        // We don't want to count whitespace-only lines as miss-indented
        .filter(|line| !line.trim().is_empty())
        .map(indentation_length)
        .min()
        .unwrap_or_default();

    DocstringLinePrinter {
        f,
        offset,
        stripped_indentation_length,
        already_normalized,
        quote_style: normalized.quotes.style,
        code_example: CodeExample::default(),
    }
    .add_iter(lines)?;

    // Same special case in the last line as for the first line
    let trim_end = docstring
        .as_ref()
        .trim_end_matches(|c: char| c.is_whitespace() && c != '\n');
    if needs_chaperone_space(normalized, trim_end) {
        space().fmt(f)?;
    }

    write!(f, [source_position(normalized.end()), normalized.quotes])
}

/// An abstraction for printing each line of a docstring.
struct DocstringLinePrinter<'ast, 'buf, 'fmt, 'src> {
    f: &'fmt mut PyFormatter<'ast, 'buf>,
    /// The source offset of the beginning of the line that is currently being
    /// printed.
    offset: TextSize,
    /// Indentation alignment based on the least indented line in the
    /// docstring.
    stripped_indentation_length: TextSize,
    /// Whether the docstring is overall already considered normalized. When it
    /// is, the formatter can take a fast path.
    already_normalized: bool,
    /// The quote style used by the docstring being printed.
    quote_style: QuoteStyle,
    /// The current code example detected in the docstring.
    code_example: CodeExample<'src>,
}

impl<'ast, 'buf, 'fmt, 'src> DocstringLinePrinter<'ast, 'buf, 'fmt, 'src> {
    /// Print all of the lines in the given iterator to this
    /// printer's formatter.
    ///
    /// Note that callers may treat the first line specially, such that the
    /// iterator given contains all lines except for the first.
    fn add_iter(
        &mut self,
        mut lines: std::iter::Peekable<std::str::Lines<'src>>,
    ) -> FormatResult<()> {
        while let Some(line) = lines.next() {
            let line = DocstringLine {
                line: Cow::Borrowed(line),
                offset: self.offset,
                is_last: lines.peek().is_none(),
            };
            // We know that the normalized string has \n line endings.
            self.offset += line.line.text_len() + "\n".text_len();
            self.add_one(line)?;
        }
        Ok(())
    }

    /// Adds the given line to this printer.
    ///
    /// Depending on what's in the line, this may or may not print the line
    /// immediately to the underlying buffer. If the line starts or is part
    /// of an existing code snippet, then the lines will get buffered until
    /// the code snippet is complete.
    fn add_one(&mut self, line: DocstringLine<'src>) -> FormatResult<()> {
        // Just pass through the line as-is without looking for a code snippet
        // when docstring code formatting is disabled. And also when we are
        // formatting a code snippet so as to avoid arbitrarily nested code
        // snippet formatting. We avoid this because it's likely quite tricky
        // to get right 100% of the time, although perhaps not impossible. It's
        // not clear that it's worth the effort to support.
        if !self.f.options().docstring_code().is_enabled() || self.f.context().docstring().is_some()
        {
            return self.print_one(&line);
        }
        match self.code_example.add(line) {
            CodeExampleAddAction::Print { original } => self.print_one(&original)?,
            CodeExampleAddAction::Kept => {}
            CodeExampleAddAction::Format {
                kind,
                code,
                original,
            } => {
                let Some(formatted_lines) = self.format(&code)? else {
                    // If formatting failed in a way that should not be
                    // allowed, we back out what we're doing and print the
                    // original lines we found as-is as if we did nothing.
                    for codeline in code {
                        self.print_one(&codeline.original)?;
                    }
                    if let Some(original) = original {
                        self.print_one(&original)?;
                    }
                    return Ok(());
                };

                self.already_normalized = false;
                match kind {
                    CodeExampleKind::Doctest(CodeExampleDoctest { indent }) => {
                        let mut lines = formatted_lines.into_iter();
                        if let Some(first) = lines.next() {
                            self.print_one(&first.map(|line| std::format!("{indent}>>> {line}")))?;
                            for docline in lines {
                                self.print_one(
                                    &docline.map(|line| std::format!("{indent}... {line}")),
                                )?;
                            }
                        }
                    }
                }
                if let Some(original) = original {
                    self.print_one(&original)?;
                }
            }
        }
        Ok(())
    }

    /// Prints the single line given.
    ///
    /// This mostly just handles indentation and ensuring line breaks are
    /// inserted as appropriate before passing it on to the formatter to
    /// print to the buffer.
    fn print_one(&mut self, line: &DocstringLine<'_>) -> FormatResult<()> {
        let trim_end = line.line.trim_end();
        if trim_end.is_empty() {
            return if line.is_last {
                // If the doc string ends with `    """`, the last line is
                // `    `, but we don't want to insert an empty line (but close
                // the docstring).
                Ok(())
            } else {
                empty_line().fmt(self.f)
            };
        }

        let tab_or_non_ascii_space = trim_end
            .chars()
            .take_while(|c| c.is_whitespace())
            .any(|c| c != ' ');

        if tab_or_non_ascii_space {
            // We strip the indentation that is shared with the docstring
            // statement, unless a line was indented less than the docstring
            // statement, in which case we strip only this much indentation to
            // implicitly pad all lines by the difference, or all lines were
            // overindented, in which case we strip the additional whitespace
            // (see example in [`format_docstring`] doc comment). We then
            // prepend the in-docstring indentation to the string.
            let indent_len = indentation_length(trim_end) - self.stripped_indentation_length;
            let in_docstring_indent = " ".repeat(usize::from(indent_len)) + trim_end.trim_start();
            text(&in_docstring_indent, Some(line.offset)).fmt(self.f)?;
        } else {
            // Take the string with the trailing whitespace removed, then also
            // skip the leading whitespace.
            let trimmed_line_range = TextRange::at(line.offset, trim_end.text_len())
                .add_start(self.stripped_indentation_length);
            if self.already_normalized {
                source_text_slice(trimmed_line_range).fmt(self.f)?;
            } else {
                // All indents are ascii spaces, so the slicing is correct.
                text(
                    &trim_end[usize::from(self.stripped_indentation_length)..],
                    Some(trimmed_line_range.start()),
                )
                .fmt(self.f)?;
            }
        }

        // We handled the case that the closing quotes are on their own line
        // above (the last line is empty except for whitespace). If they are on
        // the same line as content, we don't insert a line break.
        if !line.is_last {
            hard_line_break().fmt(self.f)?;
        }

        Ok(())
    }

    /// Given a sequence of lines from a code snippet, format them and return
    /// the formatted code as a sequence of owned docstring lines.
    ///
    /// This routine generally only returns an error when the recursive call
    /// to the formatter itself returns a `FormatError`. In all other cases
    /// (for example, if the code snippet is invalid Python or even if the
    /// resulting reformatted code snippet is invalid Python), then `Ok(None)`
    /// is returned. In this case, callers should assume that a reformatted
    /// code snippet is unavailable and bail out of trying to format it.
    ///
    /// Currently, when the above cases happen and `Ok(None)` is returned, the
    /// routine is silent about it. So from the user's perspective, this will
    /// fail silently. Ideally, this would at least emit a warning message,
    /// but at time of writing, it wasn't clear to me how to best do that.
    fn format(
        &mut self,
        code: &[CodeExampleLine<'src>],
    ) -> FormatResult<Option<Vec<DocstringLine<'static>>>> {
        use ruff_python_parser::AsMode;

        let offset = code
            .get(0)
            .expect("code blob must be non-empty")
            .original
            .offset;
        let last_line_is_last = code
            .last()
            .expect("code blob must be non-empty")
            .original
            .is_last;
        let codeblob = code
            .iter()
            .map(|line| &*line.code)
            .collect::<Vec<&str>>()
            .join("\n");
        let printed = match docstring_format_source(self.f.options(), self.quote_style, &codeblob) {
            Ok(printed) => printed,
            Err(FormatModuleError::FormatError(err)) => return Err(err),
            Err(
                FormatModuleError::LexError(_)
                | FormatModuleError::ParseError(_)
                | FormatModuleError::PrintError(_),
            ) => {
                return Ok(None);
            }
        };
        // This is a little hokey, but we want to determine whether the
        // reformatted code snippet will lead to an overall invalid docstring.
        // So attempt to parse it as Python code, but ensure it is wrapped
        // within a docstring using the same quotes as the docstring we're in
        // right now.
        //
        // This is an unfortunate stop-gap to attempt to prevent us from
        // writing invalid Python due to some oddity of the code snippet within
        // a docstring. As we fix corner cases over time, we can perhaps
        // remove this check. See the `doctest_invalid_skipped` tests in
        // `docstring_code_examples.py` for when this check is relevant.
        let wrapped = match self.quote_style {
            QuoteStyle::Single => std::format!("'''{}'''", printed.as_code()),
            QuoteStyle::Double => std::format!(r#""""{}""""#, printed.as_code()),
        };
        let result = ruff_python_parser::parse(
            &wrapped,
            self.f.options().source_type().as_mode(),
            "<filename>",
        );
        // If the resulting code is not valid, then reset and pass through
        // the docstring lines as-is.
        if result.is_err() {
            return Ok(None);
        }
        let mut lines = printed
            .as_code()
            .lines()
            .map(|line| DocstringLine {
                line: Cow::Owned(line.into()),
                offset,
                is_last: false,
            })
            .collect::<Vec<_>>();
        if let Some(last) = lines.last_mut() {
            last.is_last = last_line_is_last;
        }
        Ok(Some(lines))
    }
}

/// Represents a single line in a docstring.
///
/// This type is used to both represent the original lines in a docstring
/// (the line will be borrowed) and also the newly formatted lines from code
/// snippets (the line will be owned).
#[derive(Clone, Debug)]
struct DocstringLine<'src> {
    /// The actual text of the line, not including the line terminator.
    line: Cow<'src, str>,
    /// The offset into the source document which this line corresponds to.
    offset: TextSize,
    /// Whether this is the last line in a docstring or not. "Last" lines have
    /// some special treatment when printing.
    is_last: bool,
}

impl<'src> DocstringLine<'src> {
    /// Return this line, but with the given function applied to the text of
    /// the line.
    fn map(self, mut map: impl FnMut(&str) -> String) -> DocstringLine<'static> {
        DocstringLine {
            line: Cow::Owned(map(&self.line)),
            ..self
        }
    }
}

/// A single code example extracted from a docstring.
///
/// This represents an intermediate state from when the code example was first
/// found all the way up until the point at which the code example has finished
/// and is reformatted.
///
/// Its default state is "empty." That is, that no code example is currently
/// being collected.
#[derive(Debug, Default)]
struct CodeExample<'src> {
    /// The kind of code example being collected, or `None` if no code example
    /// has been observed.
    kind: Option<CodeExampleKind>,
    /// The lines that have been seen so far that make up the code example.
    lines: Vec<CodeExampleLine<'src>>,
}

impl<'src> CodeExample<'src> {
    /// Attempt to add an original line from a docstring to this code example.
    ///
    /// Based on the line and the internal state of whether a code example is
    /// currently being collected or not, this will return an "action" for
    /// the caller to perform. The typical case is a "print" action, which
    /// instructs the caller to just print the line as though it were not part
    /// of a code snippet.
    fn add(&mut self, original: DocstringLine<'src>) -> CodeExampleAddAction<'src> {
        match self.kind.take() {
            // There's no existing code example being built, so we look for
            // the start of one or otherwise tell the caller we couldn't find
            // anything.
            None => match self.add_start(original) {
                None => CodeExampleAddAction::Kept,
                Some(original) => CodeExampleAddAction::Print { original },
            },
            Some(CodeExampleKind::Doctest(doctest)) => {
                if let Some(code) = doctest_find_ps2_prompt(&doctest.indent, &original.line) {
                    let code = code.to_string();
                    self.lines.push(CodeExampleLine { original, code });
                    // Stay with the doctest kind while we accumulate all
                    // PS2 prompts.
                    self.kind = Some(CodeExampleKind::Doctest(doctest));
                    return CodeExampleAddAction::Kept;
                }
                let code = std::mem::take(&mut self.lines);
                let original = self.add_start(original);
                CodeExampleAddAction::Format {
                    code,
                    kind: CodeExampleKind::Doctest(doctest),
                    original,
                }
            }
        }
    }

    /// Looks for the start of a code example. If one was found, then the given
    /// line is kept and added as part of the code example. Otherwise, the line
    /// is returned unchanged and no code example was found.
    ///
    /// # Panics
    ///
    /// This panics when the existing code-example is any non-None value. That
    /// is, this routine assumes that there is no ongoing code example being
    /// collected and looks for the beginning of another code example.
    fn add_start(&mut self, original: DocstringLine<'src>) -> Option<DocstringLine<'src>> {
        assert_eq!(None, self.kind, "expected no existing code example");
        if let Some((indent, code)) = doctest_find_ps1_prompt(&original.line) {
            let indent = indent.to_string();
            let code = code.to_string();
            self.lines.push(CodeExampleLine { original, code });
            self.kind = Some(CodeExampleKind::Doctest(CodeExampleDoctest { indent }));
            return None;
        }
        Some(original)
    }
}

/// The kind of code example observed in a docstring.
#[derive(Clone, Debug, Eq, PartialEq)]
enum CodeExampleKind {
    /// Code found in Python "doctests."
    ///
    /// Documentation describing doctests and how they're recognized can be
    /// found as part of the Python standard library:
    /// https://docs.python.org/3/library/doctest.html.
    ///
    /// (You'll likely need to read the regex matching used internally by the
    /// doctest module to determine more precisely how it works.)
    Doctest(CodeExampleDoctest),
}

/// State corresponding to a single doctest code example found in a docstring.
#[derive(Clone, Debug, Eq, PartialEq)]
struct CodeExampleDoctest {
    /// The indent observed in the first doctest line.
    ///
    /// More precisely, this corresponds to the whitespace observed before
    /// the starting `>>> ` (the "PS1 prompt").
    indent: String,
}

/// A single line in a code example found in a docstring.
///
/// A code example line exists prior to formatting, and is thus in full
/// correspondence with the original lines from the docstring. Indeed, a
/// code example line includes both the original line *and* the actual code
/// extracted from the line. For example, if a line in a docstring is `>>>
/// foo(x)`, then the original line is `>>> foo(x)` and the code portion is
/// `foo(x)`.
///
/// The original line is kept for things like offset information, but also
/// because it may still be needed if it turns out that the code snippet is
/// not valid or otherwise could not be formatted. In which case, the original
/// lines are printed as-is.
#[derive(Debug)]
struct CodeExampleLine<'src> {
    /// The normalized (but original) line from the doc string. This might, for
    /// example, contain a `>>> ` or `... ` prefix if this code example is a
    /// doctest.
    original: DocstringLine<'src>,
    /// The code extracted from the line.
    code: String,
}

/// An action that a caller should perform after attempting to add a line from
/// a docstring to a code example.
///
/// Callers are expected to add every line from a docstring to a code example,
/// and the state of the code example (and the line itself) will determine
/// how the caller should react.
#[derive(Debug)]
enum CodeExampleAddAction<'src> {
    /// The line added was ignored by `CodeExample` and the caller should print
    /// it to the formatter as-is.
    ///
    /// This is the common case. That is, most lines in most docstrings are not
    /// part of a code example.
    Print { original: DocstringLine<'src> },
    /// The line added was kept by `CodeExample` as part of a new or existing
    /// code example.
    ///
    /// When this occurs, callers should not try to format the line and instead
    /// move on to the next line.
    Kept,
    /// The line added indicated that the code example is finished and should
    /// be formatted and printed. The line added is not treated as part of
    /// the code example. If the line added indicated the start of another
    /// code example, then is won't be returned to the caller here. Otherwise,
    /// callers should pass it through to the formatter as-is.
    Format {
        /// The kind of code example that was found.
        kind: CodeExampleKind,
        /// The Python code that should be formatted, indented and printed.
        ///
        /// This is guaranteed to be non-empty.
        code: Vec<CodeExampleLine<'src>>,
        /// When set, the line is considered not part of any code example
        /// and should be formatted as if the `Ignore` action were returned.
        /// Otherwise, if there is no line, then either one does not exist
        /// or it is part of another code example and should be treated as a
        /// `Kept` action.
        original: Option<DocstringLine<'src>>,
    },
}

/// Looks for a valid doctest PS1 prompt in the line given.
///
/// If one was found, then the indentation prior to the prompt is returned
/// along with the code portion of the line.
fn doctest_find_ps1_prompt(line: &str) -> Option<(&str, &str)> {
    let trim_start = line.trim_start();
    // Prompts must be followed by an ASCII space character[1].
    //
    // [1]: https://github.com/python/cpython/blob/0ff6368519ed7542ad8b443de01108690102420a/Lib/doctest.py#L809-L812
    let code = trim_start.strip_prefix(">>> ")?;
    let indent_len = line
        .len()
        .checked_sub(trim_start.len())
        .expect("suffix is <= original");
    let indent = &line[..indent_len];
    Some((indent, code))
}

/// Looks for a valid doctest PS2 prompt in the line given.
///
/// If one is found, then the code portion of the line following the PS2 prompt
/// is returned.
///
/// Callers must provide a string containing the original indentation of the
/// PS1 prompt that started the doctest containing the potential PS2 prompt
/// in the line given. If the line contains a PS2 prompt, its indentation must
/// match the indentation used for the corresponding PS1 prompt (otherwise
/// `None` will be returned).
fn doctest_find_ps2_prompt<'src>(ps1_indent: &str, line: &'src str) -> Option<&'src str> {
    let (ps2_indent, ps2_after) = line.split_once("...")?;
    // PS2 prompts must have the same indentation as their
    // corresponding PS1 prompt.[1] While the 'doctest' Python
    // module will error in this case, we just treat this line as a
    // non-doctest line.
    //
    // [1]: https://github.com/python/cpython/blob/0ff6368519ed7542ad8b443de01108690102420a/Lib/doctest.py#L733
    if ps1_indent != ps2_indent {
        return None;
    }
    // PS2 prompts must be followed by an ASCII space character unless
    // it's an otherwise empty line[1].
    //
    // [1]: https://github.com/python/cpython/blob/0ff6368519ed7542ad8b443de01108690102420a/Lib/doctest.py#L809-L812
    match ps2_after.strip_prefix(' ') {
        None if ps2_after.is_empty() => Some(""),
        None => None,
        Some(code) => Some(code),
    }
}

/// Formats the given source code using the given options.
///
/// The given quote style should correspond to the style used by the docstring
/// containing the code snippet being formatted. The formatter will use this
/// information to invert the quote style of any such strings contained within
/// the code snippet in order to avoid writing invalid Python code.
///
/// This is similar to the top-level formatting entrypoint, except this
/// explicitly sets the context to indicate that formatting is taking place
/// inside of a docstring.
fn docstring_format_source(
    options: &crate::PyFormatOptions,
    docstring_quote_style: QuoteStyle,
    source: &str,
) -> Result<Printed, FormatModuleError> {
    use ruff_python_parser::AsMode;

    let source_type = options.source_type();
    let (tokens, comment_ranges) = ruff_python_index::tokens_and_ranges(source, source_type)?;
    let module =
        ruff_python_parser::parse_ok_tokens(tokens, source, source_type.as_mode(), "<filename>")?;
    let source_code = ruff_formatter::SourceCode::new(source);
    let comments = crate::Comments::from_ast(&module, source_code, &comment_ranges);
    let locator = Locator::new(source);

    let ctx = PyFormatContext::new(options.clone(), locator.contents(), comments)
        .in_docstring(docstring_quote_style);
    let formatted = crate::format!(ctx, [module.format()])?;
    formatted
        .context()
        .comments()
        .assert_all_formatted(source_code);
    Ok(formatted.print()?)
}

/// If the last line of the docstring is `content" """` or `content\ """`, we need a chaperone space
/// that avoids `content""""` and `content\"""`. This does only applies to un-escaped backslashes,
/// so `content\\ """` doesn't need a space while `content\\\ """` does.
fn needs_chaperone_space(normalized: &NormalizedString, trim_end: &str) -> bool {
    trim_end.ends_with(normalized.quotes.style.as_char())
        || trim_end.chars().rev().take_while(|c| *c == '\\').count() % 2 == 1
}

#[cfg(test)]
mod tests {
    use crate::expression::string::indentation_length;
    use ruff_text_size::TextSize;

    #[test]
    fn test_indentation_like_black() {
        assert_eq!(indentation_length("\t \t  \t"), TextSize::new(24));
        assert_eq!(indentation_length("\t        \t"), TextSize::new(24));
        assert_eq!(indentation_length("\t\t\t"), TextSize::new(24));
        assert_eq!(indentation_length("    "), TextSize::new(4));
    }
}
