//! What the reader may call a key. A key here is a promise about every row of the table, so a
//! declaration that constrains fewer rows, or something other than the columns, is not one.

use crate::dialect::Dialect;
use crate::model::name::Name;
use crate::parser::ast;
use sqlparser::tokenizer::Token;

/// A unique index is a key only when every entry is a plain column and it has no `WHERE`. A
/// partial index constrains only the rows its predicate admits, and an entry like `lower(c)` lets
/// the plain columns beside it repeat.
pub(super) fn is_key(
    unique: bool,
    entries: &[ast::IndexColumn],
    predicate: Option<&ast::Expr>,
) -> bool {
    unique
        && predicate.is_none()
        && !entries.is_empty()
        && entries.iter().all(|e| index_column_name(e).is_some())
}

/// Whether the primary key makes its columns NOT NULL. It does on every engine but SQLite, which
/// admits NULL in a primary key column unless the table is `WITHOUT ROWID` or the key is the
/// rowid alias: one column declared exactly `INTEGER`, and not `INTEGER PRIMARY KEY DESC` (a quirk
/// SQLite keeps; `PRIMARY KEY (a DESC)` as a table constraint is the alias).
pub(super) fn primary_key_rejects_null(
    dialect: Dialect,
    ct: &ast::CreateTable,
    primary_key: &[String],
) -> bool {
    if dialect != Dialect::Sqlite || ct.without_rowid {
        return true;
    }
    let [only] = primary_key else {
        return false;
    };
    ct.columns.iter().any(|c| {
        Name::from_ident(&c.name).normalized() == *only
            && matches!(c.data_type, ast::DataType::Integer(_))
            && !c.options.iter().any(|o| {
                matches!(&o.option, ast::ColumnOption::DialectSpecific(t)
                    if t.contains(&Token::make_keyword("DESC")))
            })
    })
}

/// The simple column name an index entry refers to (`None` for functional indexes).
pub(super) fn index_column_name(ic: &ast::IndexColumn) -> Option<String> {
    match &ic.column.expr {
        ast::Expr::Identifier(i) => Some(Name::from_ident(i).normalized()),
        ast::Expr::CompoundIdentifier(parts) => {
            parts.last().map(|i| Name::from_ident(i).normalized())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::model::name::Name;
    use crate::schema::Schema;

    fn table(ddl: &str, dialect: Dialect) -> crate::schema::Table {
        let s = Schema::from_ddl_with(ddl, dialect).expect("ddl parses");
        s.table(&Name::new("t", false)).expect("table t").clone()
    }

    fn keys(ddl: &str, dialect: Dialect) -> Vec<bool> {
        table(ddl, dialect).indexes.iter().map(|i| i.key).collect()
    }

    fn nullable(ddl: &str, dialect: Dialect) -> Vec<bool> {
        table(ddl, dialect)
            .columns
            .iter()
            .map(|c| c.nullable)
            .collect()
    }

    #[test]
    fn a_partial_or_expression_unique_index_is_not_a_key() {
        let base = "CREATE TABLE t (a int, b int, c text);";
        let pg = |ix: &str| keys(&format!("{base} {ix}"), Dialect::Postgres);
        assert_eq!(pg("CREATE UNIQUE INDEX i ON t (a, b);"), [true]);
        assert_eq!(pg("CREATE UNIQUE INDEX i ON t (a) WHERE b > 0;"), [false]);
        assert_eq!(pg("CREATE UNIQUE INDEX i ON t (a, lower(c));"), [false]);
        assert_eq!(pg("CREATE INDEX i ON t (a);"), [false]);
        assert_eq!(
            keys(
                "CREATE TABLE t (a int, b int, UNIQUE (a, b))",
                Dialect::Postgres
            ),
            [true]
        );
    }

    /// MySQL accepts an expression in a unique constraint. Dropping it used to leave `UNIQUE (a)`,
    /// which marked `a` a key on its own.
    #[test]
    fn a_mysql_functional_key_part_is_not_a_key() {
        let t = table(
            "CREATE TABLE t (a int, c text, UNIQUE KEY u (a, (lower(c))))",
            Dialect::Mysql,
        );
        assert!(!t.columns[0].unique, "`a` alone is not a key");
        assert!(t.indexes.iter().all(|i| !i.key));
    }

    #[test]
    fn a_sqlite_primary_key_admits_null_unless_it_is_the_rowid_or_without_rowid() {
        let sqlite = |ddl: &str| nullable(ddl, Dialect::Sqlite);
        assert_eq!(sqlite("CREATE TABLE t (a int PRIMARY KEY)"), [true]);
        assert_eq!(sqlite("CREATE TABLE t (a text PRIMARY KEY)"), [true]);
        assert_eq!(
            sqlite("CREATE TABLE t (a integer, b int, PRIMARY KEY (a, b))"),
            [true, true]
        );
        assert_eq!(sqlite("CREATE TABLE t (a INTEGER PRIMARY KEY)"), [false]);
        assert_eq!(
            sqlite("CREATE TABLE t (a integer, PRIMARY KEY (a DESC))"),
            [false]
        );
        assert_eq!(
            sqlite("CREATE TABLE t (a text NOT NULL PRIMARY KEY)"),
            [false]
        );
        assert_eq!(
            sqlite("CREATE TABLE t (a text PRIMARY KEY) WITHOUT ROWID"),
            [false]
        );
        assert_eq!(
            sqlite("CREATE TABLE t (a INTEGER PRIMARY KEY DESC)"),
            [true]
        );
        assert_eq!(
            nullable("CREATE TABLE t (a int PRIMARY KEY)", Dialect::Postgres),
            [false]
        );
    }
}
