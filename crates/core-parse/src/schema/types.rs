//! Column types read under the dialect (CT1, `docs/phase-ct1-column-types.md`). A name the engine
//! has built in maps by kind; a name the schema declares (`CREATE TYPE`, `CREATE DOMAIN`) takes what
//! it declares; anything else is a type the engine does not have, read as `Other`.

use std::collections::HashMap;

use crate::dialect::Dialect;
use crate::model::ty::{base_name, is_builtin, Type};
use crate::parser::ast::{self, UserDefinedTypeRepresentation as Repr};

/// The types a schema declares, keyed by the last part of the name: Postgres resolves an
/// unqualified `mood` through the search path to `public.mood`, so both spellings meet.
#[derive(Default)]
pub(super) struct Declared(HashMap<String, Type>);

impl Declared {
    /// Declarations in file order, so one may use a type declared above it.
    pub(super) fn read(statements: &[ast::Statement], dialect: Dialect) -> Declared {
        let mut declared = Declared::default();
        for stmt in statements {
            let (name, ty) = match stmt {
                ast::Statement::CreateType {
                    name,
                    representation,
                } => {
                    let ty = match representation {
                        Some(Repr::Enum { .. }) => Type::Text(None),
                        Some(Repr::From { data_type, .. } | Repr::Alias { data_type }) => {
                            declared.column_type(data_type, dialect)
                        }
                        _ => Type::Other(last_part(&name.to_string())),
                    };
                    (name, ty)
                }
                ast::Statement::CreateDomain(d) => {
                    (&d.name, declared.column_type(&d.data_type, dialect))
                }
                _ => continue,
            };
            declared.0.insert(last_part(&name.to_string()), ty);
        }
        declared
    }

    /// The type of a column declared as `dt`. SQL Server's `timestamp` is `rowversion`, a binary
    /// row counter, not a date and time.
    pub(super) fn column_type(&self, dt: &ast::DataType, dialect: Dialect) -> Type {
        if matches!(dt, ast::DataType::Unspecified) {
            return Type::Unknown;
        }
        let base = base_name(dt, dialect);
        if is_builtin(dialect, &base) {
            return match (dialect, base.as_str()) {
                (Dialect::Mssql, "timestamp") => Type::Other("rowversion".into()),
                _ => Type::from_ast(dt),
            };
        }
        match self.0.get(&last_part(&base)) {
            Some(ty) => ty.clone(),
            None => Type::Other(base),
        }
    }
}

fn last_part(name: &str) -> String {
    let name = name.to_lowercase().replace(['"', '`', '[', ']'], "");
    name.rsplit('.').next().unwrap_or_default().to_string()
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::model::name::Name;
    use crate::model::ty::Type;
    use crate::schema::Schema;

    fn ty(ddl: &str, dialect: Dialect, column: &str) -> Type {
        let schema = Schema::from_ddl_with(ddl, dialect).expect("schema");
        let (_, table) = schema.tables().into_iter().next().expect("a table");
        let name = Name::new(column, false).normalized();
        let col = table.columns.iter().find(|c| c.name.normalized() == name);
        col.expect("the column").ty.clone()
    }

    #[test]
    fn interval_and_point_are_not_integers() {
        let ddl = "CREATE TABLE t (d interval, e interval day to second, p point, r int4range)";
        for c in ["d", "e", "p", "r"] {
            assert!(
                matches!(ty(ddl, Dialect::Postgres, c), Type::Other(_)),
                "{c}"
            );
        }
    }

    #[test]
    fn a_name_the_engine_lacks_is_not_read_by_its_spelling() {
        let ddl = "CREATE TABLE t (a boolean, b mint_flavor, c bit)";
        assert_eq!(ty(ddl, Dialect::Mssql, "a"), Type::Other("boolean".into()));
        assert_eq!(
            ty(ddl, Dialect::Mssql, "b"),
            Type::Other("mint_flavor".into())
        );
        assert_eq!(ty(ddl, Dialect::Postgres, "a"), Type::Boolean);
        assert_eq!(
            ty(ddl, Dialect::Sqlite, "b"),
            Type::Integer,
            "SQLite reads any name"
        );
    }

    #[test]
    fn a_declared_type_takes_its_base() {
        let ms = "CREATE TYPE boolean FROM int NOT NULL;\nCREATE TABLE t (a boolean)";
        assert_eq!(ty(ms, Dialect::Mssql, "a"), Type::Integer);
        let duck = "CREATE TYPE id_t AS INTEGER;\nCREATE TABLE t (a id_t)";
        assert_eq!(ty(duck, Dialect::Duckdb, "a"), Type::Integer);
        let pg = "CREATE TYPE public.mood AS ENUM ('a', 'b');\n\
                  CREATE DOMAIN posint AS integer CHECK (VALUE > 0);\n\
                  CREATE TABLE t (a public.mood, b mood, c posint)";
        assert_eq!(ty(pg, Dialect::Postgres, "a"), Type::Text(None));
        assert_eq!(ty(pg, Dialect::Postgres, "b"), Type::Text(None));
        assert_eq!(ty(pg, Dialect::Postgres, "c"), Type::Integer);
    }

    #[test]
    fn sql_server_spellings() {
        let ddl = "CREATE TABLE t ([a] [nvarchar](50), [b] [timestamp], c int)";
        assert_eq!(ty(ddl, Dialect::Mssql, "a"), Type::Text(None));
        assert_eq!(
            ty(ddl, Dialect::Mssql, "b"),
            Type::Other("rowversion".into())
        );
        assert_eq!(ty(ddl, Dialect::Mssql, "c"), Type::Integer);
    }

    #[test]
    fn mysql_modifiers_follow_the_type() {
        let ddl = "CREATE TABLE t (a decimal(10,2) unsigned, b int(10) unsigned)";
        assert!(matches!(ty(ddl, Dialect::Mysql, "a"), Type::Numeric { .. }));
        assert_eq!(ty(ddl, Dialect::Mysql, "b"), Type::Integer);
    }

    #[test]
    fn the_alias_forms_parse_only_on_their_engine() {
        use crate::parser::parse;
        let from = "CREATE TYPE flag FROM BIT NOT NULL";
        let alias = "CREATE TYPE id_t AS INTEGER";
        assert_eq!(parse(from, Dialect::Mssql).unwrap()[0].to_string(), from);
        assert_eq!(parse(alias, Dialect::Duckdb).unwrap()[0].to_string(), alias);
        assert!(parse(from, Dialect::Postgres).is_err());
        assert!(parse(alias, Dialect::Postgres).is_err());
    }
}
