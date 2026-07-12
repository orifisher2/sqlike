use serde::{Deserialize, Serialize};
use thiserror::Error;

use sqlparser::parser::ParserError as SqlParserError;

/// A 1-based position in the source SQL.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub line: u64,
    pub column: u64,
}

/// A failure to parse SQL.
///
/// The `message` is sqlparser's diagnostic (which usually already names the
/// position); `location` is the same position in structured form for the CLI to
/// format, and is best-effort — `None` when sqlparser doesn't report one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum ParseError {
    #[error("{message}")]
    Syntax {
        message: String,
        location: Option<Location>,
    },
}

impl ParseError {
    pub(crate) fn from_sqlparser(err: SqlParserError) -> Self {
        let message = err.to_string();
        let location = extract_location(&message);
        ParseError::Syntax { message, location }
    }
}

/// sqlparser appends `... at Line: <n>, Column: <n>` to most tokenizer and parser
/// errors. Pull the position back out of the message; return `None` if it isn't there.
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
    fn absent_position_is_none() {
        assert_eq!(extract_location("something went wrong"), None);
    }
}
