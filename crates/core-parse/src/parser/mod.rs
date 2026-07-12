//! SQL parsing — the one place VARQ ingests a raw SQL string.
//!
//! Wraps sqlparser-rs with the selected [`Dialect`] and returns its AST. Turning the
//! AST into VARQ's normalized stage tree is the stage model's job (a later phase); this
//! module stops at the AST.

mod error;

pub use error::{Location, ParseError};

// VARQ leans on sqlparser's AST types at the parser boundary; VARQ's own
// representation begins at the stage model. Re-exported so downstream modules
// reference the AST through `varq_core::parser::ast` rather than depending on the
// `sqlparser` crate name directly.
pub use sqlparser::ast;

use sqlparser::dialect::{
    Dialect as SqlDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect,
};
use sqlparser::keywords::Keyword;
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Token, TokenWithSpan, Tokenizer};

use crate::dialect::Dialect;

/// Parse a SQL string into its statements — one per `;`-separated entry — under `dialect`.
///
/// Returns a [`ParseError`] carrying a best-effort source [`Location`] on failure.
/// Never panics, whatever the input.
pub fn parse(sql: &str, dialect: Dialect) -> Result<Vec<ast::Statement>, ParseError> {
    let result = match dialect {
        Dialect::Postgres => parse_with(&PostgreSqlDialect {}, sql),
        Dialect::Mysql => parse_with(&MySqlDialect {}, sql),
        Dialect::Sqlite => parse_with(&SQLiteDialect {}, sql),
        Dialect::Mssql => parse_with(&MsSqlDialect {}, sql),
    };
    result.map_err(ParseError::from_sqlparser)
}

/// Tokenize, rewrite each value-position `?` into a positional `$N` placeholder, then parse
/// from the token stream. This is what JDBC/ODBC drivers do before handing SQL to libpq — a
/// bare `?` isn't a placeholder in the Postgres grammar (it's the jsonb existence operator),
/// so the parser would reject it. Substituting at the *token* level (not the string) keeps
/// every token's original span, so finding highlights stay accurate.
fn parse_with<D: SqlDialect>(dialect: &D, sql: &str) -> Result<Vec<ast::Statement>, ParserError> {
    let mut tokens = Tokenizer::new(dialect, sql).tokenize_with_location()?;
    anonymize_question_placeholders(&mut tokens);
    Parser::new(dialect)
        .with_tokens_with_locations(tokens)
        .parse_statements()
}

/// Rewrite each `?` that sits where a value is expected into a fresh positional placeholder,
/// leaving the jsonb operators (`?`, `?|`, `?&` between two expressions) untouched. A `?` is a
/// value when the previous significant token expects one (an operator, `(`, `,`, or a
/// value-introducing keyword like `LIMIT`); after an expression terminal it's the operator.
/// An unrecognized predecessor keeps the `?` as-is — a real parameter there surfaces as a loud
/// parse error rather than a silently mis-read jsonb query.
fn anonymize_question_placeholders(tokens: &mut [TokenWithSpan]) {
    let mut next = next_positional(tokens);
    let mut value_expected = true; // the start of input is a value position
    for tok in tokens.iter_mut() {
        match &tok.token {
            Token::Whitespace(_) => continue, // doesn't change position
            Token::Question if value_expected => {
                tok.token = Token::Placeholder(format!("${next}"));
                next += 1;
                value_expected = false; // a value just appeared
            }
            other => value_expected = expects_value_after(other),
        }
    }
}

/// One past the largest existing `$N`, so rewritten `?`s never collide with a query that
/// already mixes in positional placeholders.
fn next_positional(tokens: &[TokenWithSpan]) -> u32 {
    let mut max = 0;
    for tok in tokens {
        if let Token::Placeholder(p) = &tok.token {
            if let Some(n) = p.strip_prefix('$').and_then(|r| r.parse::<u32>().ok()) {
                max = max.max(n);
            }
        }
    }
    max + 1
}

/// Whether a value (and so possibly a `?` placeholder) can follow this token.
fn expects_value_after(t: &Token) -> bool {
    match t {
        Token::Eq
        | Token::Neq
        | Token::Lt
        | Token::LtEq
        | Token::Gt
        | Token::GtEq
        | Token::Spaceship
        | Token::Plus
        | Token::Minus
        | Token::Mul
        | Token::Div
        | Token::Mod
        | Token::StringConcat
        | Token::LParen
        | Token::Comma
        | Token::LBracket
        | Token::Question
        | Token::QuestionPipe
        | Token::QuestionAnd => true,
        Token::Word(w) => matches!(
            w.keyword,
            Keyword::AND
                | Keyword::OR
                | Keyword::NOT
                | Keyword::IN
                | Keyword::LIKE
                | Keyword::ILIKE
                | Keyword::BETWEEN
                | Keyword::IS
                | Keyword::LIMIT
                | Keyword::OFFSET
                | Keyword::VALUES
                | Keyword::WHEN
                | Keyword::THEN
                | Keyword::ELSE
                | Keyword::WHERE
                | Keyword::HAVING
                | Keyword::ON
                | Keyword::SELECT
                | Keyword::SET
                | Keyword::RETURNING
                | Keyword::ALL
                | Keyword::ANY
                | Keyword::SOME
                | Keyword::ESCAPE
        ),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_select() {
        let stmts = parse(
            "SELECT id, email FROM users WHERE id = 1",
            Dialect::Postgres,
        )
        .unwrap();
        assert_eq!(stmts.len(), 1);
    }

    #[test]
    fn splits_multiple_statements() {
        let stmts = parse("SELECT 1; SELECT 2;", Dialect::Postgres).unwrap();
        assert_eq!(stmts.len(), 2);
    }

    #[test]
    fn dialect_selects_the_grammar() {
        // Backtick-quoted identifiers are MySQL syntax; Postgres rejects them. Proves the
        // dialect argument actually switches grammars rather than being ignored.
        let sql = "SELECT `id` FROM `users`";
        assert!(parse(sql, Dialect::Mysql).is_ok());
        assert!(parse(sql, Dialect::Postgres).is_err());
    }

    #[test]
    fn reports_a_location_on_tokenizer_error() {
        // An unterminated string literal fails in the tokenizer, which always
        // reports a position — so the location must be present.
        let err = parse("SELECT 'oops", Dialect::Postgres).unwrap_err();
        let ParseError::Syntax { location, message } = err;
        assert!(!message.is_empty());
        assert!(location.is_some(), "expected a location, got: {message}");
    }

    #[test]
    fn never_panics_on_garbage() {
        // These must return (Ok or Err), never panic.
        for d in [Dialect::Postgres, Dialect::Mysql] {
            let _ = parse("", d);
            let _ = parse("SELECT", d);
            let _ = parse("!@#$%^&*()", d);
            let _ = parse("DROP DROP DROP", d);
        }
    }

    /// Debug-render the (single) parsed statement.
    fn dbg(sql: &str) -> String {
        let stmts = parse(sql, Dialect::Postgres).unwrap();
        assert_eq!(stmts.len(), 1);
        format!("{:?}", stmts[0])
    }

    #[test]
    fn rewrites_value_position_question_mark() {
        // A bare `?` (which the Postgres grammar rejects) becomes a positional placeholder.
        assert!(dbg("SELECT * FROM t WHERE id = ?").contains("Placeholder(\"$1\")"));
    }

    #[test]
    fn each_question_mark_is_a_distinct_positional() {
        let d = dbg("SELECT * FROM t WHERE a = ? OR b = ?");
        assert!(
            d.contains("Placeholder(\"$1\")") && d.contains("Placeholder(\"$2\")"),
            "{d}"
        );
    }

    #[test]
    fn question_mark_in_limit_offset_and_in_list() {
        let d = dbg("SELECT * FROM t WHERE id IN (?, ?) LIMIT ? OFFSET ?");
        for n in 1..=4 {
            assert!(
                d.contains(&format!("Placeholder(\"${n}\")")),
                "missing ${n}: {d}"
            );
        }
    }

    #[test]
    fn jsonb_question_operator_is_left_alone() {
        // `data ? 'k'` is the jsonb existence operator (a value precedes it), not a parameter.
        assert!(!dbg("SELECT * FROM t WHERE data ? 'k'").contains("Placeholder"));
        // `?|` and `?&` are distinct tokens and never touched.
        assert!(!dbg("SELECT * FROM t WHERE tags ?| array['a']").contains("Placeholder"));
    }

    #[test]
    fn rewritten_numbering_skips_existing_positionals() {
        // Mixing `$1` with `?` must not reuse $1.
        let d = dbg("SELECT * FROM t WHERE a = $1 AND b = ?");
        assert!(d.contains("Placeholder(\"$2\")"), "{d}");
    }

    #[test]
    fn native_placeholders_parse_unchanged() {
        for sql in [
            "SELECT * FROM t WHERE id = $1",
            "SELECT * FROM t WHERE id = :name",
            "SELECT * FROM t WHERE id = $name",
        ] {
            assert!(parse(sql, Dialect::Postgres).is_ok(), "{sql}");
        }
    }

    #[test]
    fn token_substitution_preserves_spans() {
        // Substituting the token kind (not the SQL string) keeps every span at its original
        // position: the `?` stays a 1-column span, and nothing downstream of it shifts.
        let sql = "SELECT * FROM t WHERE id = ?";
        let col = sql.find('?').unwrap() + 1; // 1-based column
        let d = dbg(sql);
        assert!(
            d.contains(&format!("Location(1,{col})..Location(1,{})", col + 1)),
            "placeholder span should sit on the original `?` at col {col}: {d}"
        );
    }
}
