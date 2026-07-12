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
}

impl fmt::Display for Dialect {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Dialect::Postgres => "postgres",
            Dialect::Mysql => "mysql",
            Dialect::Sqlite => "sqlite",
            Dialect::Mssql => "mssql",
        })
    }
}
