//! Tokenizing a schema file, which is not only DDL the grammar reads.

use super::{rewrite, Assigner, TokenizeError};
use crate::dialect::Dialect;
use crate::schema::{looks_like_create_table, statements};

/// The schema's payload. A real schema file (a `pg_dump`) carries statements the grammar cannot
/// read (a psql `\restrict` line, `ALTER TYPE ... OWNER TO`) and statements holding text the
/// tokenizer cannot place (`COMMENT ON ... IS '...'`). The server's reader already skips the first
/// kind statement by statement (`schema::from_ddl`), so failing the whole file here only failed the
/// call on the client. When the whole file does not tokenize, each statement is tried on its own,
/// with the reader's cuts, and one that fails is left out of the payload, which only means it never
/// leaves the machine. A `CREATE TABLE` that fails stays an error: the table would be missing
/// without a word.
pub(super) fn rewrite_schema(
    sql: &str,
    dialect: Dialect,
    a: &mut Assigner,
) -> Result<String, TokenizeError> {
    let whole = match rewrite(sql, dialect, a) {
        Ok((payload, _)) => return Ok(payload),
        Err(e) => e,
    };
    let mut kept = Vec::new();
    for piece in statements(sql) {
        match rewrite(&piece, dialect, a) {
            Ok((payload, _)) => kept.push(payload.trim().to_string()),
            Err(e) if looks_like_create_table(&piece) => return Err(e),
            Err(_) => {}
        }
    }
    if kept.is_empty() {
        return Err(whole);
    }
    Ok(kept.join(";\n") + ";")
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::tokenize::{tokenize, TokenizeError};

    const DUMP: &str = "\\restrict s3cretKey
SET statement_timeout = 0;
CREATE TYPE public.plan_tier AS ENUM ('free', 'pro');
ALTER TYPE public.plan_tier OWNER TO postgres;
CREATE TABLE public.accounts (account_id bigint NOT NULL, tier public.plan_tier);
ALTER TABLE ONLY public.accounts ADD CONSTRAINT accounts_pkey PRIMARY KEY (account_id);
COMMENT ON TABLE public.accounts IS 'class: core | holds pii';
\\unrestrict s3cretKey
";

    #[test]
    fn a_dump_with_unreadable_statements_tokenizes() {
        let t = tokenize(
            "SELECT account_id FROM accounts",
            Some(DUMP),
            Dialect::Postgres,
        )
        .expect("a pg_dump schema tokenizes");
        let schema = t.schema_payload.unwrap();
        for private in [
            "s3cretKey",
            "accounts",
            "account_id",
            "postgres",
            "restrict",
            "pii",
        ] {
            assert!(!schema.contains(private), "{private} leaked: {schema}");
        }
        assert!(
            schema.contains("CREATE TABLE"),
            "the table is kept: {schema}"
        );
        assert!(schema.contains("PRIMARY KEY"), "the key is kept: {schema}");
        // And the server reads it as the key, under the tokenized names.
        let read = crate::schema::Schema::from_ddl(&schema).expect("payload reads");
        let keyed: Vec<usize> = read
            .tables()
            .iter()
            .map(|(_, t)| t.primary_key.len())
            .collect();
        assert_eq!(keyed, [1], "{schema}");
    }

    #[test]
    fn a_readable_schema_is_tokenized_whole_as_before() {
        let ddl = "CREATE TABLE t (a int);\nCREATE INDEX t_a ON t (a);";
        let t = tokenize("SELECT a FROM t", Some(ddl), Dialect::Postgres).unwrap();
        assert_eq!(t.schema_payload.unwrap().lines().count(), 2);
    }

    #[test]
    fn an_unreadable_create_table_is_still_an_error() {
        let ddl = "SET x = 1;\nCREATE TABLE t (a int;\nCREATE TABLE u (b int);";
        assert!(matches!(
            tokenize("SELECT b FROM u", Some(ddl), Dialect::Postgres),
            Err(TokenizeError::Parse(_))
        ));
    }

    #[test]
    fn a_file_with_nothing_readable_is_still_an_error() {
        assert!(matches!(
            tokenize(
                "SELECT 1",
                Some("\\restrict k\nnot ddl at all"),
                Dialect::Postgres
            ),
            Err(TokenizeError::Parse(_))
        ));
    }
}
