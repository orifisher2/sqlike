//! The SQL dialect an analysis targets.
//!
//! Postgres is the default and the only empirically-verified dialect today; MySQL's
//! per-rule severities and applicability are filled in by the MySQL verification phase.
//! A rule is one cross-DB *shape* with a per-dialect verdict, so the dialect threads
//! from [`crate::analyze_with`] down to each rule's `severity(Dialect)`.

use std::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    #[default]
    Postgres,
    Mysql,
    Sqlite,
    Mssql,
    Mariadb,
    /// The first non-row-store dialect. Most of the catalog's mechanisms are "this defeats an
    /// index", and DuckDB has no general-purpose secondary index to defeat, so a verdict inherited
    /// from a row store is a guess here in a way it was not for MariaDB.
    ///
    /// DD1 gives DuckDB its own arm at every site, valued from Postgres and tagged
    /// `// DD1 provisional`. That tag means **assumed, never measured** — DD3 replaces each one
    /// with a real DuckDB measurement, and `docs/rules-v0.2.md` carries the count that has to reach
    /// zero before DuckDB is advertised anywhere.
    Duckdb,
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dialect::Postgres => "postgres",
            Dialect::Mysql => "mysql",
            Dialect::Sqlite => "sqlite",
            Dialect::Mssql => "mssql",
            Dialect::Mariadb => "mariadb",
            Dialect::Duckdb => "duckdb",
        })
    }
}
