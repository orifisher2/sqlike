//! Text the AST walk never sees. Comments and optimizer hints are not AST nodes, and some values
//! sit in plain-`String` fields the walk cannot tell from engine vocabulary (`ALTER USER … PASSWORD`,
//! `COPY` paths, MySQL column `COMMENT`). Both used to pass into the payload untouched.
//!
//! Comments are erased before the edits are applied: nothing downstream reads them, and a masked
//! comment would still reveal its length and position. A string the walk did not reach (an `ENUM`
//! or `SET` value, a MySQL `COMMENT`) becomes a sentinel like any other string: refusing it instead
//! refused every `mysqldump` schema (UR1, E). The finished payload is still re-lexed and refused if
//! it holds a comment or a string nothing wrote. What neither can see is a word or a number in such
//! a field: it looks like any other word or number, and only the test corpus covers it.

use sqlparser::tokenizer::{Token, TokenWithSpan, Whitespace};

use super::{Kind, Lines, LitShape, Raw, TokenizeError};
use crate::dialect::Dialect;

/// One erasing edit per comment in `sql`. A line comment's token holds its newline, which goes
/// back in so the lines (and the spans counted in them) stay where they were.
pub(super) fn comments(
    sql: &str,
    dialect: Dialect,
    lines: &Lines,
) -> Result<Vec<Raw>, TokenizeError> {
    let mut out = Vec::new();
    for t in lex(sql, dialect)? {
        if !is_comment(&t.token) {
            continue;
        }
        let (start, end) = range(&t, sql, lines);
        let keep = if sql[start..end].ends_with('\n') {
            "\n"
        } else {
            " "
        };
        out.push(Raw {
            start,
            end,
            kind: Kind::Erased(keep),
        });
    }
    Ok(out)
}

/// One sentinel edit per string in `sql` that no edit in `claimed` covers: the strings in fields the
/// walk does not reach. A wildcard-only string carries nothing and stays, as it does in the walk.
pub(super) fn strings(
    sql: &str,
    dialect: Dialect,
    lines: &Lines,
    claimed: &[Raw],
) -> Result<Vec<Raw>, TokenizeError> {
    let mut out = Vec::new();
    for t in lex(sql, dialect)? {
        let Some(text) = string_text(&t.token) else {
            continue;
        };
        let (start, end) = range(&t, sql, lines);
        if text.chars().all(|c| matches!(c, '%' | '_' | '\\'))
            || claimed.iter().any(|r| start < r.end && r.start < end)
        {
            continue;
        }
        out.push(Raw {
            start,
            end,
            kind: Kind::Literal {
                source: sql[start..end].to_string(),
                shape: LitShape::Str {
                    lead: false,
                    trail: false,
                    has_wild: false,
                },
            },
        });
    }
    Ok(out)
}

/// Refuse a payload that still holds a comment, or a string literal that is neither one of the
/// sentinels this rewrite wrote (`sentinels`, payload byte ranges) nor wildcard-only, which is the
/// one kind of string kept on purpose.
pub(super) fn check(
    payload: &str,
    dialect: Dialect,
    sentinels: &[(usize, usize)],
) -> Result<(), TokenizeError> {
    let lines = Lines::of(payload);
    for t in lex(payload, dialect)? {
        if is_comment(&t.token) {
            return Err(TokenizeError::Unlocatable("a comment"));
        }
        let Some(text) = string_text(&t.token) else {
            continue;
        };
        let wildcards = text.chars().all(|c| matches!(c, '%' | '_' | '\\'));
        if !wildcards && !sentinels.contains(&range(&t, payload, &lines)) {
            return Err(TokenizeError::Unlocatable("a string value"));
        }
    }
    Ok(())
}

fn lex(sql: &str, dialect: Dialect) -> Result<Vec<TokenWithSpan>, TokenizeError> {
    crate::parser::lex(sql, dialect).map_err(|e| TokenizeError::Internal(e.to_string()))
}

fn range(t: &TokenWithSpan, s: &str, lines: &Lines) -> (usize, usize) {
    let (a, b) = (t.span.start, t.span.end);
    (
        lines.byte_at(s, a.line, a.column),
        lines.byte_at(s, b.line, b.column),
    )
}

fn is_comment(t: &Token) -> bool {
    matches!(
        t,
        Token::Whitespace(Whitespace::SingleLineComment { .. } | Whitespace::MultiLineComment(_))
    )
}

fn string_text(t: &Token) -> Option<&str> {
    use Token::*;
    match t {
        SingleQuotedString(s)
        | DoubleQuotedString(s)
        | TripleSingleQuotedString(s)
        | TripleDoubleQuotedString(s)
        | SingleQuotedByteStringLiteral(s)
        | DoubleQuotedByteStringLiteral(s)
        | TripleSingleQuotedByteStringLiteral(s)
        | TripleDoubleQuotedByteStringLiteral(s)
        | SingleQuotedRawStringLiteral(s)
        | DoubleQuotedRawStringLiteral(s)
        | TripleSingleQuotedRawStringLiteral(s)
        | TripleDoubleQuotedRawStringLiteral(s)
        | NationalStringLiteral(s)
        | EscapedStringLiteral(s)
        | UnicodeStringLiteral(s)
        | HexStringLiteral(s) => Some(s),
        DollarQuotedString(d) => Some(&d.value),
        QuoteDelimitedStringLiteral(q) | NationalQuoteDelimitedStringLiteral(q) => Some(&q.value),
        _ => None,
    }
}
