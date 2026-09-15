use serde::{Deserialize, Serialize};
use thiserror::Error;

use sqlparser::parser::ParserError as SqlParserError;
use sqlparser::tokenizer::{Location as SqlLocation, Span as SqlSpan, Token};

use crate::model::Span;

/// A 1-based position in the source SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub line: u64,
    pub column: u64,
}

/// A failure to parse SQL.
///
/// `message` is the parser's own diagnostic, unchanged. `kind` and `span` carry the same facts as
/// data, read from the parser's structured variants rather than back out of the prose, so a
/// consumer can say what was expected, what was found, and exactly where.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("{message}")]
    Syntax {
        kind: ParseErrorKind,
        /// The failing token's extent when the parser reported one, a point when it reported only
        /// a position, `None` when it reported neither.
        span: Option<Span>,
        message: String,
    },
}

/// What the parser reported, as far as it has structure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseErrorKind {
    /// The parser expected one thing and found a token.
    Expected { expected: String, found: FoundToken },
    /// A message with a position: the tokenizer, or a parser site that names no expectation.
    Message,
    /// A message with no structure at all.
    Other,
}

/// The token the parser stopped on, as text plus a coarse kind. Recorded this way rather than as
/// the parser's own token type so the parser's internals stay out of this crate's public error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoundToken {
    pub text: String,
    pub kind: TokenKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An identifier or keyword. `keyword` is true when the parser recognises the word.
    Word {
        keyword: bool,
    },
    Number,
    StringLiteral,
    Punctuation,
    Eof,
    Other,
}

impl ParseError {
    pub fn location(&self) -> Option<Location> {
        let ParseError::Syntax { span, .. } = self;
        span.map(|s| s.start)
    }

    pub(crate) fn from_sqlparser(err: SqlParserError) -> Self {
        let message = err.to_string();
        let (kind, span) = match &err {
            SqlParserError::Expected { expected, found } => (
                ParseErrorKind::Expected {
                    expected: expected.clone(),
                    found: FoundToken::of(&found.token),
                },
                conv_span(found.span),
            ),
            SqlParserError::At { location, .. } => (ParseErrorKind::Message, point(*location)),
            // The parser's remaining string sites carry no structure. A handful embed another
            // error's text, which can include a position, so the old prose fallback stays for
            // exactly those.
            _ => (
                ParseErrorKind::Other,
                extract_location(&message).map(|l| Span::new(l, l)),
            ),
        };
        ParseError::Syntax {
            kind,
            span,
            message,
        }
    }
}

impl FoundToken {
    fn of(token: &Token) -> Self {
        let kind = match token {
            Token::Word(w) => TokenKind::Word {
                keyword: w.keyword != sqlparser::keywords::Keyword::NoKeyword,
            },
            Token::Number(..) => TokenKind::Number,
            Token::SingleQuotedString(_)
            | Token::DoubleQuotedString(_)
            | Token::TripleSingleQuotedString(_)
            | Token::TripleDoubleQuotedString(_)
            | Token::NationalStringLiteral(_)
            | Token::EscapedStringLiteral(_)
            | Token::UnicodeStringLiteral(_)
            | Token::HexStringLiteral(_)
            | Token::DollarQuotedString(_) => TokenKind::StringLiteral,
            Token::EOF => TokenKind::Eof,
            Token::Whitespace(_) | Token::Placeholder(_) | Token::CustomBinaryOperator(_) => {
                TokenKind::Other
            }
            _ => TokenKind::Punctuation,
        };
        FoundToken {
            text: token.to_string(),
            kind,
        }
    }
}

/// The parser uses line 0 to mean "no position".
fn conv_loc(l: SqlLocation) -> Option<Location> {
    (l.line != 0).then_some(Location {
        line: l.line,
        column: l.column,
    })
}

fn point(l: SqlLocation) -> Option<Span> {
    conv_loc(l).map(|l| Span::new(l, l))
}

fn conv_span(s: SqlSpan) -> Option<Span> {
    let start = conv_loc(s.start)?;
    let end = conv_loc(s.end).unwrap_or(start);
    Some(Span::new(start, end))
}

/// The parser appends `... at Line: <n>, Column: <n>` to a message it built from another error.
/// Only the unstructured `Other` kind goes through here.
fn extract_location(message: &str) -> Option<Location> {
    let line = number_after(message, "Line: ")?;
    let column = number_after(message, "Column: ")?;
    Some(Location { line, column })
}

fn number_after(haystack: &str, needle: &str) -> Option<u64> {
    let start = haystack.find(needle)? + needle.len();
    let digits: String = haystack[start..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_a_position() {
        let loc = extract_location("Unterminated string literal at Line: 3, Column: 12");
        assert_eq!(
            loc,
            Some(Location {
                line: 3,
                column: 12
            })
        );
    }

    #[test]
    fn no_position_is_none() {
        assert_eq!(extract_location("recursion limit exceeded"), None);
    }
}
