//! SQL parsing — the one place VARQ ingests a raw SQL string.
//!
//! Wraps sqlparser-rs with the selected [`Dialect`] and returns its AST. Turning the
//! AST into VARQ's normalized stage tree is the stage model's job (a later phase); this
//! module stops at the AST.

mod diagnose;
mod error;

pub use diagnose::{diagnose, parse_error_finding, parse_failure_result, Cause, Diagnosis};
pub use error::{FoundToken, Location, ParseError, ParseErrorKind, TokenKind};

// VARQ leans on sqlparser's AST types at the parser boundary; VARQ's own
// representation begins at the stage model. Re-exported so downstream modules
// reference the AST through `varq_core::parser::ast` rather than depending on the
// `sqlparser` crate name directly.
pub use sqlparser::ast;

use sqlparser::dialect::{
    Dialect as SqlDialect, DuckDbDialect, MsSqlDialect, MySqlDialect, PostgreSqlDialect,
    SQLiteDialect,
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
        // sqlparser ships no MariaDB grammar; MySQL's is the correct approximation. MariaDB-only
        // syntax (SEQUENCE, RETURNING on DELETE, OFFSET … FETCH) may not parse — recorded as a
        // known limitation, revisited only on dogfooding pain.
        Dialect::Mariadb => parse_with(&MySqlDialect {}, sql),
        // A real grammar, unlike MariaDB's approximation above. `ASOF JOIN … ON` and `USING SAMPLE`
        // are the two DuckDB shapes it does not accept — a clean parse error, which is the right
        // failure: a refusal to answer rather than a wrong answer.
        Dialect::Duckdb => parse_with(&DuckDbDialect {}, sql),
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

    /// PG1c. `a JOIN b JOIN c ON P1 ON P2` is a nested join with its ON clauses deferred: every
    /// engine but SQLite reads it as `a JOIN (b JOIN c ON P1) ON P2` (right-nested), measured in
    /// `docs/phase-pg1c-nested-join-on.md`. The shape matters because for an outer join the nesting
    /// changes which rows survive, so we assert the tree, not just that it parses.
    #[test]
    fn deferred_on_nested_join_is_right_nested() {
        use ast::{SetExpr, Statement, TableFactor};
        let sql = "SELECT 1 FROM a JOIN b JOIN c ON b.y = c.y ON a.x = b.x";
        for d in [
            Dialect::Postgres,
            Dialect::Mysql,
            Dialect::Mariadb,
            Dialect::Mssql,
            Dialect::Duckdb,
        ] {
            let stmts = parse(sql, d).unwrap_or_else(|e| panic!("{d:?}: {e}"));
            let Statement::Query(q) = &stmts[0] else {
                panic!("{d:?}: not a query");
            };
            let SetExpr::Select(select) = &*q.body else {
                panic!("{d:?}: not a select");
            };
            let joins = &select.from[0].joins;
            assert_eq!(joins.len(), 1, "{d:?}: c should nest under b, not sit flat");
            assert!(
                matches!(joins[0].relation, TableFactor::NestedJoin { .. }),
                "{d:?}: the join's right side should be a nested (b JOIN c) tree",
            );
        }
        // SQLite rejects the construct outright, so we do too: a refusal, not a wrong tree.
        assert!(parse(sql, Dialect::Sqlite).is_err());
    }

    #[test]
    fn reports_a_location_on_tokenizer_error() {
        // An unterminated string literal fails in the tokenizer, which always
        // reports a position — so the location must be present.
        let err = parse("SELECT 'oops", Dialect::Postgres).unwrap_err();
        let location = err.location();
        let ParseError::Syntax { message, .. } = &err;
        assert!(!message.is_empty());
        assert!(location.is_some(), "expected a location, got: {message}");
    }

    /// The structure the parser now hands over, read as data rather than out of the prose.
    #[test]
    fn expected_errors_carry_the_token_and_its_span() {
        use super::{ParseErrorKind, TokenKind};
        let err = parse("SELECT a FROM t WHERE", Dialect::Postgres).unwrap_err();
        let ParseError::Syntax {
            kind,
            span,
            message,
        } = &err;
        let ParseErrorKind::Expected { expected, found } = kind else {
            panic!("expected an Expected kind, got {kind:?}: {message}");
        };
        assert!(!expected.is_empty());
        assert_eq!(found.kind, TokenKind::Eof);
        assert_eq!(err.location(), span.map(|s| s.start));

        // A found token has an extent (the parser's end is exclusive), which the old regex could
        // never say: it recovered a start column and nothing else.
        let err = parse("SELECT 1 2 3", Dialect::Postgres).unwrap_err();
        let ParseError::Syntax { kind, span, .. } = &err;
        let ParseErrorKind::Expected { expected, found } = kind else {
            panic!("expected an Expected kind");
        };
        assert_eq!(expected, "end of statement");
        assert_eq!(found.text, "2");
        assert_eq!(found.kind, TokenKind::Number);
        let span = span.expect("a found token has a span");
        assert_eq!((span.start.column, span.end.column), (10, 11));
    }

    #[test]
    fn tokenizer_errors_carry_a_point() {
        use super::ParseErrorKind;
        let err = parse("SELECT 'oops", Dialect::Postgres).unwrap_err();
        let ParseError::Syntax { kind, span, .. } = &err;
        assert_eq!(*kind, ParseErrorKind::Message);
        let span = span.expect("the tokenizer reports a position");
        assert_eq!(span.start, span.end);
    }

    /// PG1b: parentheses around a lone table factor, gated per dialect to exactly what each engine
    /// accepts (measured on real engines). The soundness property is that we never parse a shape
    /// the engine rejects; coverage losses (engine yes, us no) are allowed.
    #[test]
    fn parenthesised_lone_table_factor_matches_the_engines() {
        let ok = |sql: &str, d: Dialect| parse(sql, d).is_ok();
        // MySQL and MariaDB: a lone table in parens, but not alias-after-parens or a paren derived.
        for d in [Dialect::Mysql, Dialect::Mariadb] {
            assert!(ok("SELECT * FROM (t)", d), "{d:?} FROM (t)");
            assert!(ok("SELECT * FROM ((t))", d), "{d:?} FROM ((t))");
            assert!(ok("SELECT * FROM (t a)", d), "{d:?} FROM (t a)");
            assert!(
                !ok("SELECT * FROM (t) a", d),
                "{d:?} must reject FROM (t) a"
            );
            assert!(
                !ok("SELECT * FROM ((SELECT 1) x)", d),
                "{d:?} must reject paren-derived"
            );
        }
        // SQLite accepts every shape the broad flag enables.
        assert!(ok("SELECT * FROM (t)", Dialect::Sqlite));
        assert!(ok("SELECT * FROM (t) a", Dialect::Sqlite));
        assert!(ok("SELECT * FROM ((SELECT 1) x)", Dialect::Sqlite));
        // Postgres, SQL Server and DuckDB reject a lone parenthesised table entirely.
        for d in [Dialect::Postgres, Dialect::Mssql, Dialect::Duckdb] {
            assert!(!ok("SELECT * FROM (t)", d), "{d:?} must reject FROM (t)");
        }
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
