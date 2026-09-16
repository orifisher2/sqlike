//! Explain a parse failure (phase PG2).
//!
//! A parse failure is one of three different claims, and the difference is the whole point:
//!
//! - [`Cause::Mistake`]: the SQL has a recognisable error, and we say what it is.
//! - [`Cause::Unsupported`]: the SQL uses a construct real engines accept and our parser does not.
//!   We name the construct and say the query is fine.
//! - [`Cause::Unknown`]: the parser stopped and neither of the above applies. We say what it saw
//!   and assert nothing about whose fault that is.
//!
//! A rule that cannot tell must produce `Unknown`. Telling a user their correct query is broken is
//! the wrong answer this module exists to remove, and it is worse than an unspecific one.
//!
//! The `Unsupported` table is derived from the parse census (`crates/verify/tests/parse_census.rs`),
//! not written from imagination, and each entry names the dialects that accept the construct. An
//! entry only matches when the selected dialect is one of them: window-frame `EXCLUDE` is valid
//! Postgres and not valid MySQL, and a MySQL user who writes it has made a mistake.

use crate::dialect::Dialect;
use crate::model::Span;
use crate::result::{AnalysisResult, Finding, Severity};

use super::{Location, ParseError, ParseErrorKind, TokenKind};

/// What a parse failure means, in words a person can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnosis {
    pub location: Option<Location>,
    pub span: Option<Span>,
    pub cause: Cause,
    /// One sentence, always present, with the position in front when there is one.
    pub headline: String,
    /// The reason or the suggestion, when there is one.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A construct the census shows real engines accept and we do not.
    Unsupported {
        construct: &'static str,
        dialects: &'static [Dialect],
    },
    /// A recognisable error in the SQL.
    Mistake,
    /// The parser stopped and nothing above applies.
    Unknown,
}

pub fn diagnose(sql: &str, err: &ParseError, dialect: Dialect) -> Diagnosis {
    let ParseError::Syntax {
        kind,
        span,
        message,
    } = err;
    let cx = Context::new(sql, kind, *span, message);

    if let Some(d) = unsupported(&cx, dialect) {
        return d;
    }
    if let Some(d) = mistake(&cx, dialect) {
        return d;
    }
    unknown(&cx)
}

/// Everything a rule may look at: the error, and the text around where it happened.
struct Context<'a> {
    kind: &'a ParseErrorKind,
    span: Option<Span>,
    message: &'a str,
    /// The parser's `expected` phrase, or empty.
    expected: &'a str,
    /// The found token's text, or empty.
    found: &'a str,
    found_kind: Option<TokenKind>,
    /// Words before the failure, nearest first, with punctuation split off.
    before: Vec<String>,
    /// Words from the failure onward, nearest first.
    after: Vec<String>,
    /// Whether the character just before the failure (ignoring whitespace) is a comma.
    comma_before: bool,
    /// Unbalanced brackets outside string literals, if any.
    brackets: Brackets,
}

enum Brackets {
    Balanced,
    Unclosed(Location),
    Unopened(Location),
}

impl<'a> Context<'a> {
    fn new(sql: &'a str, kind: &'a ParseErrorKind, span: Option<Span>, message: &'a str) -> Self {
        let (expected, found, found_kind) = match kind {
            ParseErrorKind::Expected { expected, found } => {
                (expected.as_str(), found.text.as_str(), Some(found.kind))
            }
            _ => ("", "", None),
        };
        let at = span.map(|s| offset_of(sql, s.start)).unwrap_or(sql.len());
        let head = &sql[..at];
        let tail = &sql[at..];
        let before: Vec<String> = words(head).into_iter().rev().collect();
        let after = words(tail);
        let comma_before = head.trim_end().ends_with(',');
        Context {
            kind,
            span,
            message,
            expected,
            found,
            found_kind,
            before,
            after,
            comma_before,
            brackets: brackets(sql),
        }
    }

    fn before_is(&self, n: usize, word: &str) -> bool {
        self.before
            .get(n)
            .is_some_and(|w| w.eq_ignore_ascii_case(word))
    }

    fn found_is(&self, word: &str) -> bool {
        self.found.eq_ignore_ascii_case(word)
    }

    fn position(&self) -> String {
        match self.span {
            Some(s) => format!("Line {}, column {}: ", s.start.line, s.start.column),
            None => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Unsupported: constructs from the census, keyed on what the parser reports for each.
// ---------------------------------------------------------------------------------------------

const PG: &[Dialect] = &[Dialect::Postgres];
const MY: &[Dialect] = &[Dialect::Mysql, Dialect::Mariadb];
const PG_SQLITE_DUCK: &[Dialect] = &[Dialect::Postgres, Dialect::Sqlite, Dialect::Duckdb];
const ALL_BUT_SQLITE: &[Dialect] = &[
    Dialect::Postgres,
    Dialect::Mysql,
    Dialect::Mariadb,
    Dialect::Mssql,
    Dialect::Duckdb,
];

fn unsupported(cx: &Context, dialect: Dialect) -> Option<Diagnosis> {
    let end = cx.expected == "end of statement";
    let entry: Option<(&'static str, &'static [Dialect])> = if cx.found_is("EXCLUDE")
        && cx.expected == ")"
    {
        Some(("the window frame EXCLUDE clause", PG_SQLITE_DUCK))
    } else if cx.found_is("ON") && end && count_word(&cx.before, "JOIN") >= 2 {
        Some((
            "a nested JOIN whose ON conditions come after the joined tables",
            ALL_BUT_SQLITE,
        ))
    } else if cx.found_is("USING")
        && end
        && cx.before.iter().any(|w| w.eq_ignore_ascii_case("ORDER"))
    {
        Some(("ORDER BY with a USING operator", PG))
    } else if cx.found_is("FROM") && end && cx.before_is(0, "ROWS") {
        Some(("ROWS FROM in the FROM clause", PG))
    } else if (cx.found_is("SEARCH") || cx.found_is("CYCLE")) && cx.expected.contains("query body")
    {
        Some(("SEARCH or CYCLE on a recursive query", PG))
    } else if cx.before_is(0, "VARIADIC") && cx.expected == ")" {
        Some(("a VARIADIC argument", PG))
    } else if cx.found_is("*") && end && cx.before_is(1, "FROM") {
        Some(("table inheritance, a table name followed by *", PG))
    } else if cx.found_is("SIMILAR") && cx.before_is(1, "SUBSTRING") {
        Some(("SUBSTRING with a SIMILAR pattern", PG))
    } else if cx.found_is("PASSING") && cx.expected == ")" {
        Some(("an XML function with a PASSING clause", PG))
    } else if cx.found_is("JSON") && cx.before_is(0, "IS") {
        Some(("the IS JSON predicate", PG))
    } else if cx.expected.contains("after '.'") && cx.before_is(0, "*") && cx.before_is(1, ")") {
        Some(("expanding a composite value with (expr).*", PG))
    } else if cx.found.starts_with('@') && end && cx.before_is(0, "INTO") {
        Some(("SELECT ... INTO a user variable", MY))
    } else if cx.found_is("IN") && end && cx.before_is(0, "LOCK") {
        Some(("LOCK IN SHARE MODE", MY))
    } else if end && cx.before_is(0, "MOD") && cx.found_kind == Some(TokenKind::Number) {
        Some(("MOD written as an infix operator", MY))
    } else if cx.found_is("CHARACTER") && cx.expected == ")" && cx.before_is(0, "CHAR") {
        Some(("CAST to CHAR with a CHARACTER SET", MY))
    } else {
        None
    };

    let (construct, dialects) = entry?;
    if !dialects.contains(&dialect) {
        return None;
    }
    let name = dialect_name(dialect);
    Some(Diagnosis {
        location: cx.span.map(|s| s.start),
        span: cx.span,
        cause: Cause::Unsupported {
            construct,
            dialects,
        },
        headline: format!(
            "{}this uses {construct}, which sqlike does not support yet.",
            cx.position()
        ),
        detail: Some(format!(
            "Your query is valid {name}. This is a gap in sqlike's parser, not a problem with the query."
        )),
    })
}

// ---------------------------------------------------------------------------------------------
// Mistake: the handful of errors a parser can be certain about.
// ---------------------------------------------------------------------------------------------

/// Keywords that begin a statement. `SELCT` is not a statement, and this is the list it is
/// compared against.
const STATEMENT_KEYWORDS: &[&str] = &[
    "SELECT", "WITH", "INSERT", "UPDATE", "DELETE", "CREATE", "ALTER", "DROP", "EXPLAIN",
];

/// Keywords that begin a clause. When the parser reads a misspelt one as an alias and fails on
/// the word after it, the misspelling is the word before the failure.
const CLAUSE_KEYWORDS: &[&str] = &[
    "FROM", "WHERE", "GROUP", "ORDER", "HAVING", "LIMIT", "OFFSET", "JOIN", "ON", "UNION", "WINDOW",
];

/// Dialects whose select list accepts a trailing comma. A trailing comma is not a mistake there,
/// and if our parser rejected one it would be our gap, not the user's error.
const TRAILING_COMMA_OK: &[Dialect] = &[Dialect::Duckdb];

fn mistake(cx: &Context, dialect: Dialect) -> Option<Diagnosis> {
    let at = cx.position();
    let mk = |headline: String, detail: Option<String>| Diagnosis {
        location: cx.span.map(|s| s.start),
        span: cx.span,
        cause: Cause::Mistake,
        headline,
        detail,
    };

    // An unterminated string is certain: the tokenizer says so, and there is nothing else it can be.
    if *cx.kind == ParseErrorKind::Message && cx.message.contains("Unterminated string") {
        return Some(mk(
            format!("{at}the string starting here is never closed."),
            None,
        ));
    }

    // A misspelt statement keyword: the very first word is not a statement.
    if cx.expected == "an SQL statement"
        && cx.found_kind == Some(TokenKind::Word { keyword: false })
    {
        if let Some(meant) = nearest_keyword(cx.found, STATEMENT_KEYWORDS) {
            return Some(mk(
                format!("{at}`{}` is not a SQL keyword.", cx.found),
                Some(format!("Did you mean `{meant}`?")),
            ));
        }
    }

    // A misspelt clause keyword, read as an alias, so the parser fails on the word after it.
    if cx.expected == "end of statement" {
        if let Some(prev) = cx.before.first() {
            let is_word = prev.chars().all(|c| c.is_alphanumeric() || c == '_');
            if is_word {
                if let Some(meant) = nearest_keyword(prev, CLAUSE_KEYWORDS) {
                    return Some(mk(
                        format!("{at}unexpected `{}` here.", cx.found),
                        Some(format!(
                            "The word before it, `{prev}`, is not a SQL keyword. Did you mean `{meant}`?"
                        )),
                    ));
                }
            }
        }
        // GROUP or ORDER not followed by BY.
        if (cx.found_is("GROUP") || cx.found_is("ORDER"))
            && !cx
                .after
                .get(1)
                .is_some_and(|w| w.eq_ignore_ascii_case("BY"))
        {
            let next = cx.after.get(1).cloned().unwrap_or_default();
            return Some(mk(
                format!(
                    "{at}`{}` must be followed by `BY`.",
                    cx.found.to_uppercase()
                ),
                (!next.is_empty()).then(|| format!("Found `{next}` instead.")),
            ));
        }
    }

    // A trailing comma: a comma, then a clause keyword or a closing bracket where an item should
    // be. A comma followed by something we merely cannot parse (`\N`, `{fn ...}`) is not this,
    // and the first version of this rule claimed it was.
    let next_is_clause_or_close = cx
        .after
        .first()
        .is_some_and(|w| w == ")" || CLAUSE_KEYWORDS.iter().any(|k| w.eq_ignore_ascii_case(k)));
    if cx.comma_before
        && next_is_clause_or_close
        && !TRAILING_COMMA_OK.contains(&dialect)
        && (cx.message.contains("Expected an expression") || cx.expected == "an expression")
    {
        let next = cx.after.first().cloned().unwrap_or_default();
        return Some(mk(
            format!("{at}unexpected `{next}` after a comma."),
            Some(format!(
                "The comma before `{next}` says another item follows, and none does. Remove the trailing comma."
            )),
        ));
    }

    // Brackets, counted outside string literals, so the claim is about the whole statement.
    match cx.brackets {
        Brackets::Unclosed(open) if cx.expected == ")" => {
            return Some(mk(
                format!("{at}unexpected `{}`; a `)` is missing.", cx.found),
                Some(format!(
                    "The `(` at line {}, column {} is never closed.",
                    open.line, open.column
                )),
            ));
        }
        Brackets::Unopened(close) if cx.found == ")" => {
            return Some(mk(
                format!(
                    "Line {}, column {}: unmatched `)`.",
                    close.line, close.column
                ),
                Some("There is no `(` for it to close.".to_string()),
            ));
        }
        _ => {}
    }

    None
}

/// The closest keyword by edit distance, with the distance allowed scaling with the keyword:
/// 1 for keywords of four letters or fewer, 2 beyond. Distance 2 from a two-letter keyword such as
/// `AS` or `IN` matches almost any short identifier, which is a guess, not a diagnosis.
fn nearest_keyword(word: &str, keywords: &[&'static str]) -> Option<&'static str> {
    let w = word.to_ascii_uppercase();
    keywords
        .iter()
        .map(|k| (edit_distance(&w, k), *k))
        .filter(|(d, k)| *d > 0 && *d <= if k.len() <= 4 { 1 } else { 2 })
        .min_by_key(|(d, _)| *d)
        .map(|(_, k)| k)
}

/// Edit distance counting an adjacent transposition as one edit (optimal string alignment).
/// Plain Levenshtein charges two for `FORM` against `FROM`, and swapped letters are the most
/// common typo there is.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            d[i][j] = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                d[i][j] = d[i][j].min(d[i - 2][j - 2] + 1);
            }
        }
    }
    d[n][m]
}

// ---------------------------------------------------------------------------------------------
// Unknown: what the parser saw, and nothing more.
// ---------------------------------------------------------------------------------------------

fn unknown(cx: &Context) -> Diagnosis {
    let at = cx.position();
    let headline = match cx.kind {
        ParseErrorKind::Expected { .. } if cx.found_kind == Some(TokenKind::Eof) => {
            format!(
                "{at}the statement ends here, but sqlike expected {}.",
                expected_phrase(cx.expected)
            )
        }
        ParseErrorKind::Expected { .. } => format!(
            "{at}unexpected `{}` here. sqlike expected {}.",
            cx.found,
            expected_phrase(cx.expected)
        ),
        _ => {
            // The parser's own sentence, minus its prefix; it is already a sentence.
            let m = cx.message.trim_start_matches("sql parser error: ");
            format!("{at}{m}.")
        }
    };
    Diagnosis {
        location: cx.span.map(|s| s.start),
        span: cx.span,
        cause: Cause::Unknown,
        headline,
        detail: None,
    }
}

/// The parser's `expected` phrases are mostly readable as they are. The exceptions are bare
/// punctuation and a few internal phrasings.
fn expected_phrase(expected: &str) -> String {
    match expected {
        ")" => "a closing `)`".to_string(),
        "(" => "an opening `(`".to_string(),
        "," => "a comma".to_string(),
        "end of statement" => "the end of the statement".to_string(),
        "an SQL statement" => "a statement".to_string(),
        other => other.to_string(),
    }
}

// ---------------------------------------------------------------------------------------------
// The finding every front door shows.
// ---------------------------------------------------------------------------------------------

/// The `parse-error` finding for a failure, explained. `message` is the diagnosis headline,
/// `reasoning` places the fault or says we cannot, `suggestion` is the fix when there is one.
/// The parser's own text is never shown: for a construct we do not support it reads as "your
/// query is broken", which is false.
pub fn parse_error_finding(sql: &str, err: &ParseError, dialect: Dialect) -> Finding {
    let d = diagnose(sql, err, dialect);
    let (reasoning, suggestion) = match &d.cause {
        Cause::Unsupported { .. } => (d.detail.clone(), None),
        Cause::Mistake => (
            Some(
                "The statement does not parse as written, so nothing else can be analyzed until \
                 it does."
                    .to_string(),
            ),
            d.detail.clone(),
        ),
        Cause::Unknown => (
            Some(
                "sqlike could not read the statement past this point. That may be a mistake in \
                 the query, or a construct sqlike does not support yet."
                    .to_string(),
            ),
            None,
        ),
    };
    Finding {
        rule: "parse-error".into(),
        severity: Severity::High,
        message: d.headline,
        span: d.span,
        suggestion,
        reasoning,
        fix: None,
        edits: Vec::new(),
        subject: None,
    }
}

/// A whole analysis result for a query that did not parse: the one finding and nothing else.
/// This is what the server returns for such a query, and since the diagnosis needs only the SQL
/// and the parser, a client can produce the identical result locally and send nothing.
pub fn parse_failure_result(sql: &str, err: &ParseError, dialect: Dialect) -> AnalysisResult {
    AnalysisResult {
        findings: vec![parse_error_finding(sql, err, dialect)],
        advice: Vec::new(),
        hotspots: Vec::new(),
        parameters: Vec::new(),
        dialect,
        advice_hypothetical: true,
    }
}

// ---------------------------------------------------------------------------------------------
// Text helpers.
// ---------------------------------------------------------------------------------------------

fn dialect_name(d: Dialect) -> &'static str {
    match d {
        Dialect::Postgres => "Postgres",
        Dialect::Mysql => "MySQL",
        Dialect::Mariadb => "MariaDB",
        Dialect::Sqlite => "SQLite",
        Dialect::Mssql => "SQL Server",
        Dialect::Duckdb => "DuckDB",
    }
}

/// Byte offset of a 1-based line and column. Columns count characters, as the parser does.
fn offset_of(sql: &str, loc: Location) -> usize {
    let mut line = 1;
    let mut col = 1;
    for (i, c) in sql.char_indices() {
        if line == loc.line && col == loc.column {
            return i;
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    sql.len()
}

/// Words and single punctuation marks, in order. `concat(VARIADIC` becomes `concat`, `(`,
/// `VARIADIC`, so a rule can look at the word before a bracket.
fn words(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in text.chars() {
        if c.is_alphanumeric() || c == '_' || c == '@' || c == '$' || c == '.' {
            cur.push(c);
        } else {
            if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
            if !c.is_whitespace() {
                out.push(c.to_string());
            }
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

fn count_word(words: &[String], word: &str) -> usize {
    words
        .iter()
        .filter(|w| w.eq_ignore_ascii_case(word))
        .count()
}

/// Bracket balance outside single-quoted strings, with the position of the first unmatched one.
///
/// Whether a backslash escapes a quote depends on the dialect and on session settings (MySQL:
/// yes; Postgres: only in `E'...'` strings). Rather than guess, a statement with a backslash
/// before a quote is not counted at all: no claim beats a wrong one.
fn brackets(sql: &str) -> Brackets {
    if sql.contains("\\'") || sql.contains("\\\"") {
        return Brackets::Balanced;
    }
    let mut stack: Vec<Location> = Vec::new();
    let mut in_string = false;
    let (mut line, mut col) = (1u64, 1u64);
    for c in sql.chars() {
        match c {
            '\'' => in_string = !in_string,
            '(' if !in_string => stack.push(Location { line, column: col }),
            ')' if !in_string && stack.pop().is_none() => {
                return Brackets::Unopened(Location { line, column: col });
            }
            _ => {}
        }
        if c == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    match stack.first() {
        Some(open) => Brackets::Unclosed(*open),
        None => Brackets::Balanced,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse;

    fn diag(sql: &str, d: Dialect) -> Diagnosis {
        let err = parse(sql, d).unwrap_err();
        diagnose(sql, &err, d)
    }

    #[test]
    fn misspelt_statement_keyword_is_a_mistake_with_a_suggestion() {
        let d = diag("SELCT a FROM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.headline,
            "Line 1, column 1: `SELCT` is not a SQL keyword."
        );
        assert_eq!(d.detail.as_deref(), Some("Did you mean `SELECT`?"));
    }

    #[test]
    fn misspelt_clause_keyword_is_found_one_word_back() {
        let d = diag("SELECT a FORM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.detail.as_deref(),
            Some("The word before it, `FORM`, is not a SQL keyword. Did you mean `FROM`?")
        );
        let d = diag("SELECT a FROM t WHER a = 1", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert!(d
            .detail
            .as_deref()
            .unwrap()
            .ends_with("Did you mean `WHERE`?"));
    }

    #[test]
    fn short_keywords_do_not_match_at_distance_two() {
        // `t` is one edit from `ON` only if we allow distance 2 on a two-letter keyword; we do not.
        assert_eq!(nearest_keyword("xy", &["ON", "AS"]), None);
        assert_eq!(nearest_keyword("FORM", &["FROM"]), Some("FROM"));
        assert_eq!(nearest_keyword("WHRE", &["WHERE"]), Some("WHERE"));
    }

    #[test]
    fn trailing_comma_is_a_mistake_where_the_dialect_rejects_it() {
        let d = diag("SELECT a, b, FROM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.headline,
            "Line 1, column 14: unexpected `FROM` after a comma."
        );
        // DuckDB accepts a trailing comma, and our parser does too, so there is nothing to diagnose.
        assert!(parse("SELECT a, b, FROM t", Dialect::Duckdb).is_ok());
    }

    #[test]
    fn brackets_are_counted_outside_strings() {
        let d = diag("SELECT (a + b FROM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.detail.as_deref(),
            Some("The `(` at line 1, column 8 is never closed.")
        );
        let d = diag("SELECT a) FROM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(d.headline, "Line 1, column 9: unmatched `)`.");
        assert!(matches!(brackets("SELECT '(' FROM t"), Brackets::Balanced));
        // A backslash before a quote means the string boundaries are dialect-dependent, so no claim.
        assert!(matches!(
            brackets("SELECT 'a\\'( FROM t"),
            Brackets::Balanced
        ));
    }

    #[test]
    fn unterminated_string_is_a_mistake() {
        let d = diag("SELECT 'abc FROM t", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.headline,
            "Line 1, column 8: the string starting here is never closed."
        );
    }

    #[test]
    fn group_without_by() {
        let d = diag("SELECT a FROM t GROUP B a", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Mistake);
        assert_eq!(
            d.headline,
            "Line 1, column 17: `GROUP` must be followed by `BY`."
        );
    }

    #[test]
    fn window_exclude_is_ours_on_postgres_and_a_mistake_nowhere() {
        let sql = "SELECT sum(x) OVER (ORDER BY y ROWS BETWEEN 1 PRECEDING AND 1 FOLLOWING EXCLUDE TIES) FROM t";
        let d = diag(sql, Dialect::Postgres);
        assert!(matches!(
            d.cause,
            Cause::Unsupported {
                construct: "the window frame EXCLUDE clause",
                ..
            }
        ));
        assert_eq!(
            d.headline,
            "Line 1, column 73: this uses the window frame EXCLUDE clause, which sqlike does not support yet."
        );
        assert_eq!(
            d.detail.as_deref(),
            Some("Your query is valid Postgres. This is a gap in sqlike's parser, not a problem with the query.")
        );
        // MySQL has no EXCLUDE clause. The entry does not match, and nothing claims a mistake either.
        let d = diag(sql, Dialect::Mysql);
        assert_eq!(d.cause, Cause::Unknown);
    }

    #[test]
    fn census_constructs_are_named_for_their_dialects() {
        let cases: [(&str, Dialect, &str); 8] = [
            ("SELECT 1 FROM a JOIN b JOIN c ON b.id = c.id ON a.id = b.id", Dialect::Postgres, "a nested JOIN whose ON conditions come after the joined tables"),
            ("SELECT a FROM t ORDER BY a USING <", Dialect::Postgres, "ORDER BY with a USING operator"),
            ("WITH RECURSIVE s(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM s) SEARCH DEPTH FIRST BY x SET o SELECT * FROM s", Dialect::Postgres, "SEARCH or CYCLE on a recursive query"),
            ("SELECT concat(VARIADIC ARRAY['a','b'])", Dialect::Postgres, "a VARIADIC argument"),
            ("SELECT * FROM t* WHERE a = 1", Dialect::Postgres, "table inheritance, a table name followed by *"),
            ("SELECT a FROM t INTO @v", Dialect::Mysql, "SELECT ... INTO a user variable"),
            ("SELECT a FROM t LOCK IN SHARE MODE", Dialect::Mariadb, "LOCK IN SHARE MODE"),
            ("SELECT CAST(a AS CHAR CHARACTER SET utf8mb4) FROM t", Dialect::Mysql, "CAST to CHAR with a CHARACTER SET"),
        ];
        for (sql, d, want) in cases {
            let got = diag(sql, d);
            match got.cause {
                Cause::Unsupported { construct, .. } => assert_eq!(construct, want, "{sql}"),
                other => panic!(
                    "{sql}: expected Unsupported, got {other:?}: {}",
                    got.headline
                ),
            }
        }
    }

    #[test]
    fn a_comma_before_something_unparseable_is_not_a_trailing_comma() {
        // Found in MySQL's suite: `\N` is MySQL's NULL literal, `{fn ...}` is ODBC escape syntax.
        // Both follow a comma, and neither is a trailing comma.
        for sql in [
            "select null,\\N,isnull(null)",
            "select {fn length(\"hello\")}, { date \"1997-10-20\" }",
        ] {
            let d = diag(sql, Dialect::Mysql);
            assert_ne!(d.cause, Cause::Mistake, "{sql}: {}", d.headline);
        }
    }

    /// PG2's second gate, the mirror of the census one: a fixture of genuine mistakes, none of
    /// which may come back as our gap. Most should be recognised as mistakes; that count is
    /// reported, and the hard line is zero `Unsupported`.
    #[test]
    fn genuine_mistakes_are_never_called_our_gap() {
        let fixture: &[&str] = &[
            // misspelt keywords
            "SELCT a FROM t",
            "SELET a FROM t",
            "SLECT a FROM t",
            "SELECT a FORM t",
            "SELECT a FRM t",
            "SELECT a FROM t WHER a = 1",
            "SELECT a FROM t WHRE a = 1",
            "SELECT a FROM t GROUP B a",
            "SELECT a FROM t ORDER B a",
            "SELECT a FROM t GORUP BY a",
            "SELECT a FROM t ORDR BY a",
            "SELECT a FROM t HAVNG count(*) > 1",
            // unbalanced brackets
            "SELECT (a + b FROM t",
            "SELECT a FROM t WHERE (a = 1",
            "SELECT count(a FROM t",
            "SELECT a) FROM t",
            "SELECT a FROM t WHERE a = 1)",
            "SELECT a FROM t WHERE (a = 1))",
            "SELECT ((a) FROM t",
            "SELECT a FROM (SELECT b FROM t",
            "SELECT a FROM t WHERE a IN (1, 2",
            "SELECT coalesce(a, b FROM t",
            "SELECT a FROM t WHERE (a = 1 AND (b = 2)",
            "SELECT a, (SELECT max(b) FROM u FROM t",
            // unterminated strings
            "SELECT 'abc FROM t",
            "SELECT a FROM t WHERE b = 'x",
            "SELECT 'a', 'b FROM t",
            "SELECT a FROM t WHERE b LIKE '%x",
            "SELECT a FROM t WHERE b = 'it''s",
            "SELECT ''''",
            "SELECT a FROM t WHERE b = 'a\nb",
            "SELECT concat('a', 'b) FROM t",
            "SELECT 'unterminated",
            "SELECT a FROM t WHERE b IN ('x', 'y",
            "SELECT a FROM t ORDER BY 'z",
            "SELECT a AS 'alias FROM t",
            // trailing commas
            "SELECT a, b, FROM t",
            "SELECT a, FROM t",
            "SELECT a FROM t WHERE a IN (1, 2,)",
            "SELECT a FROM t GROUP BY a, ",
            "SELECT a FROM t ORDER BY a, ",
            "SELECT count(a,) FROM t",
            "SELECT a, b, c, FROM t WHERE a = 1",
            "SELECT a FROM t, ",
            "INSERT INTO t (a, b,) VALUES (1, 2)",
            "SELECT a FROM t GROUP BY a, HAVING count(*) > 1",
            "SELECT coalesce(a, b,) FROM t",
            "SELECT a, b, FROM t ORDER BY a",
        ];
        let (mut recognised, mut unknown, mut blamed_on_us) = (0, 0, Vec::new());
        for sql in fixture {
            for d in [
                Dialect::Postgres,
                Dialect::Mysql,
                Dialect::Sqlite,
                Dialect::Mssql,
            ] {
                let Err(err) = parse(sql, d) else { continue };
                match diagnose(sql, &err, d).cause {
                    Cause::Mistake => recognised += 1,
                    Cause::Unknown => unknown += 1,
                    Cause::Unsupported { construct, .. } => {
                        blamed_on_us.push(format!("{d:?}: {sql} -> {construct}"))
                    }
                }
            }
        }
        println!(
            "mistake fixture: {recognised} recognised, {unknown} unknown, {} called our gap",
            blamed_on_us.len()
        );
        assert!(
            blamed_on_us.is_empty(),
            "genuine mistakes diagnosed as our gap:\n{}",
            blamed_on_us.join("\n")
        );
        assert!(
            recognised > unknown,
            "the rules recognise too few of their own fixture: {recognised} vs {unknown}"
        );
    }

    #[test]
    fn ambiguous_shapes_stay_unknown() {
        // A trailing `HAVING` with nothing after it: the parser stops at end of input with no
        // recognisable mistake and no construct we can name, so it is not claimed either way.
        let d = diag("SELECT a FROM t GROUP BY a HAVING", Dialect::Postgres);
        assert_eq!(d.cause, Cause::Unknown);
        assert!(
            d.headline.starts_with("the statement ends here"),
            "{}",
            d.headline
        );
    }
}
