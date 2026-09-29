//! DDL → [`Schema`]. Reads `CREATE TABLE` (columns, constraints) and `CREATE INDEX`
//! via sqlparser; ignores any other statement.

use thiserror::Error;

use crate::dialect::Dialect;
use crate::model::name::{Name, TableName};
use crate::model::ty::Type;
use crate::parser::{self, ast};

use super::{Column, ForeignKey, Index, Schema, Table};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SchemaError {
    #[error("could not parse schema DDL: {0}")]
    Parse(String),
}

pub(super) fn from_ddl(sql: &str, dialect: Dialect) -> Result<Schema, SchemaError> {
    let mut partial = false;
    let statements = match parser::parse(sql, dialect) {
        Ok(statements) => statements,
        // A real schema file is not only `CREATE TABLE`: a `pg_dump` carries `SET`, `COMMENT ON`,
        // `GRANT`, a `DO` block, a trigger. Parsing the file as one unit meant the first statement
        // the grammar could not read discarded **every** table with it, and the caller analyzed
        // schema-less with one medium finding to explain it. Statement by statement, what cannot be
        // read is skipped and the tables around it survive.
        Err(whole_file) => match statement_by_statement(sql, dialect) {
            Some((statements, lost_a_table)) => {
                partial = lost_a_table;
                statements
            }
            None => return Err(SchemaError::Parse(whole_file.to_string())),
        },
    };
    let mut schema = Schema::default();

    // Tables first, then indexes (which attach to already-built tables).
    for stmt in &statements {
        if let ast::Statement::CreateTable(ct) = stmt {
            schema.insert(build_table(ct));
        }
    }
    for stmt in &statements {
        if let ast::Statement::CreateIndex(ci) = stmt {
            apply_index(&mut schema, ci);
        }
    }
    if partial {
        schema.mark_partial();
    }
    Ok(schema)
}

/// The statements of `sql` that parse, cut on T-SQL batch separators and then on the semicolons the
/// splitter can find, with a flag for whether a statement that was going to define a table was lost.
///
/// `None` when nothing parses, so a caller that passed something which is not DDL at all still gets
/// the parse error it would have got before, rather than a silent empty schema.
fn statement_by_statement(sql: &str, dialect: Dialect) -> Option<(Vec<ast::Statement>, bool)> {
    let (mut out, mut lost_a_table) = (Vec::new(), false);
    for piece in split_batches(sql).into_iter().flat_map(split_semicolons) {
        if piece.trim().is_empty() {
            continue;
        }
        match parser::parse(&piece, dialect) {
            Ok(statements) => out.extend(statements),
            Err(_) => lost_a_table |= looks_like_create_table(&piece),
        }
    }
    (!out.is_empty()).then_some((out, lost_a_table))
}

/// The batches of `sql`, split on T-SQL's bare `GO` line.
///
/// `GO` is not a statement and carries no semicolon, so to a splitter that only knows `;` a script
/// that uses it reads as one enormous statement: the `SET`/`GO` preamble is glued to the
/// `CREATE TABLE` under it and both are lost together. That cost 11 of the 102 tables in the
/// corpus's SQL Server port, one per file, on top of the 14 the grammar genuinely cannot read.
fn split_batches(sql: &str) -> Vec<&str> {
    let (mut out, mut start, mut at) = (Vec::new(), 0usize, 0usize);
    for line in sql.split_inclusive('\n') {
        if line.trim().eq_ignore_ascii_case("GO") {
            out.push(&sql[start..at]);
            start = at + line.len();
        }
        at += line.len();
    }
    out.push(&sql[start..]);
    out
}

/// Whether a statement the grammar could not read was going to define a table.
///
/// This is what decides whether the schema is *incomplete* or merely *lossy*. A `GRANT`, a
/// `COMMENT ON` or a `DO` block that does not parse costs nothing the analysis reads, and the table
/// list is still whole, so `unknown-table` keeps working. A `CREATE TABLE` that does not parse means
/// a table the user has is missing from the schema, and claiming it does not exist would be a
/// false accusation. Every real schema file has some of the first kind.
fn looks_like_create_table(piece: &str) -> bool {
    let head: String = piece
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .take(1)
        .collect::<String>()
        .to_uppercase();
    head.starts_with("CREATE") && head.contains("TABLE")
}

/// Split on the semicolons outside a string, a dollar-quoted body or a line comment. Deliberately
/// small: it only has to find statement ends well enough that each piece parses on its own, and a
/// piece it gets wrong is skipped rather than believed.
fn split_semicolons(sql: &str) -> Vec<String> {
    let (mut out, mut cur) = (Vec::new(), String::new());
    let (mut quote, mut dollar, mut comment) = (None::<char>, false, false);
    let bytes: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        let next = bytes.get(i + 1).copied();
        if comment {
            if c == '\n' {
                comment = false;
            }
        } else if let Some(q) = quote {
            if c == q {
                quote = None;
            }
        } else if dollar {
            if c == '$' && next == Some('$') {
                dollar = false;
                cur.push(c);
                i += 1;
                cur.push('$');
                i += 1;
                continue;
            }
        } else if c == '-' && next == Some('-') {
            comment = true;
        } else if c == '\'' || c == '"' || c == '`' {
            quote = Some(c);
        } else if c == '$' && next == Some('$') {
            dollar = true;
            cur.push(c);
            i += 1;
            cur.push('$');
            i += 1;
            continue;
        } else if c == ';' {
            out.push(std::mem::take(&mut cur));
            i += 1;
            continue;
        }
        cur.push(c);
        i += 1;
    }
    out.push(cur);
    out
}

fn build_table(ct: &ast::CreateTable) -> Table {
    let mut columns = Vec::new();
    let mut primary_key = Vec::new();
    let mut foreign_keys = Vec::new();

    for col in &ct.columns {
        let col_name = Name::from_ident(&col.name);
        let mut nullable = true;
        let mut unique = false;
        for opt in &col.options {
            match &opt.option {
                ast::ColumnOption::NotNull => nullable = false,
                ast::ColumnOption::Null => nullable = true,
                ast::ColumnOption::Unique(_) => unique = true,
                ast::ColumnOption::PrimaryKey(_) => {
                    unique = true;
                    nullable = false;
                    primary_key.push(col_name.normalized());
                }
                ast::ColumnOption::ForeignKey(fk) => foreign_keys.push(ForeignKey {
                    columns: vec![col_name.normalized()],
                    ref_table: TableName::from_object_name(&fk.foreign_table)
                        .name
                        .normalized(),
                    ref_columns: fk.referred_columns.iter().map(norm_ident).collect(),
                }),
                _ => {}
            }
        }
        columns.push(Column {
            name: col_name,
            ty: Type::from_ast(&col.data_type),
            nullable,
            unique,
        });
    }

    let mut indexes = Vec::new();
    for c in &ct.constraints {
        match c {
            ast::TableConstraint::PrimaryKey(pk) => {
                for ic in &pk.columns {
                    if let Some(n) = index_column_name(ic) {
                        primary_key.push(n);
                    }
                }
            }
            // `Column::unique` says the column alone is a key. A key over several columns is
            // not that (each may repeat), so it is kept whole, as the unique index every engine
            // backs it with.
            ast::TableConstraint::Unique(u) => {
                let cols: Vec<String> = u.columns.iter().filter_map(index_column_name).collect();
                match cols.as_slice() {
                    [one] => mark_unique(&mut columns, one),
                    _ => indexes.push(Index {
                        name: None,
                        columns: cols,
                        include: Vec::new(),
                        unique: true,
                    }),
                }
            }
            ast::TableConstraint::ForeignKey(fk) => foreign_keys.push(ForeignKey {
                columns: fk.columns.iter().map(norm_ident).collect(),
                ref_table: TableName::from_object_name(&fk.foreign_table)
                    .name
                    .normalized(),
                ref_columns: fk.referred_columns.iter().map(norm_ident).collect(),
            }),
            _ => {}
        }
    }

    // Primary-key columns are NOT NULL; the key is unique as a whole, so a column of it is a
    // key on its own only when it is the whole key.
    for pk in &primary_key {
        if let Some(c) = columns.iter_mut().find(|c| &c.name.normalized() == pk) {
            c.nullable = false;
            c.unique |= primary_key.len() == 1;
        }
    }

    Table {
        name: TableName::from_object_name(&ct.name),
        columns,
        primary_key,
        indexes,
        foreign_keys,
    }
}

fn apply_index(schema: &mut Schema, ci: &ast::CreateIndex) {
    let table_name = TableName::from_object_name(&ci.table_name).name;
    let columns: Vec<String> = ci.columns.iter().filter_map(index_column_name).collect();
    let index = Index {
        name: ci.name.as_ref().map(crate::model::name::object_name_last),
        columns,
        include: ci.include.iter().map(norm_ident).collect(),
        unique: ci.unique,
    };
    if let Some(t) = schema.table_mut(&table_name) {
        t.indexes.push(index);
    }
}

fn mark_unique(columns: &mut [Column], normalized: &str) {
    if let Some(c) = columns
        .iter_mut()
        .find(|c| c.name.normalized() == normalized)
    {
        c.unique = true;
    }
}

fn norm_ident(ident: &ast::Ident) -> String {
    Name::from_ident(ident).normalized()
}

/// The simple column name an index entry refers to (`None` for functional indexes).
fn index_column_name(ic: &ast::IndexColumn) -> Option<String> {
    match &ic.column.expr {
        ast::Expr::Identifier(i) => Some(norm_ident(i)),
        ast::Expr::CompoundIdentifier(parts) => parts.last().map(norm_ident),
        _ => None,
    }
}

#[cfg(test)]
mod reader_tests {
    use super::*;

    /// A real schema file is not only `CREATE TABLE`, and the statements around them must not take
    /// the tables down with them. This is the `pg_dump` shape: comments, a `SET`, a `DO` block.
    #[test]
    fn a_statement_the_grammar_cannot_read_costs_only_itself() {
        let ddl = "\
            SET client_min_messages = warning;\n\
            DO $$ BEGIN PERFORM 1; END $$;\n\
            CREATE TABLE users (id int PRIMARY KEY, email text);\n\
            COMMENT ON TABLE users IS 'people';\n\
            CREATE INDEX ix_users__email ON users (email);\n";
        let schema = from_ddl(ddl, Dialect::Postgres).expect("the tables survive");
        let t = schema
            .table(&Name::new("users", false))
            .expect("the table around the unreadable statements");
        assert_eq!(t.columns.len(), 2);
        assert!(
            !schema.is_partial(),
            "nothing that defines a table was lost, so absence is still evidence"
        );
    }

    /// When a `CREATE TABLE` itself cannot be read, the schema is missing a table the user has, and
    /// the resolver must not call that table unknown.
    #[test]
    fn an_unreadable_create_table_makes_the_schema_partial() {
        let ddl = "\
            CREATE TABLE ok (id int PRIMARY KEY);\n\
            CREATE TABLE odd (id int, total AS (1 + 2) PERSISTED);\n";
        let schema = from_ddl(ddl, Dialect::Postgres).expect("the readable table survives");
        assert!(schema.table(&Name::new("ok", false)).is_some());
        assert!(
            schema.table(&Name::new("odd", false)).is_none(),
            "it could not be read"
        );
        assert!(schema.is_partial(), "so the schema knows it is incomplete");
    }

    /// T-SQL ends a batch with a bare `GO` and no semicolon. Splitting on `;` alone glued the
    /// preamble to the table under it and lost both, one table per file.
    #[test]
    fn a_go_batch_separator_does_not_swallow_the_table_under_it() {
        let ddl = "\
            SET QUOTED_IDENTIFIER ON;\n\
            GO\n\
            \n\
            CREATE TABLE cards (card_id bigint PRIMARY KEY, customer_id bigint NOT NULL);\n";
        let schema = from_ddl(ddl, Dialect::Mssql).expect("the table after the GO");
        assert!(schema.table(&Name::new("cards", false)).is_some());
    }

    /// Something that is not DDL at all still fails, rather than yielding an empty schema that
    /// silently explains nothing.
    #[test]
    fn a_file_with_nothing_readable_is_still_an_error() {
        assert!(from_ddl("not sql at all;", Dialect::Postgres).is_err());
    }
}
