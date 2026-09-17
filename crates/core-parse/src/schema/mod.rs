//! Schema representation and DDL parsing.
//!
//! A [`Schema`] is built from `CREATE TABLE` / `CREATE INDEX` DDL and used by the
//! resolver (F4) to check column existence and annotate types. Pure data — no I/O.

mod parse;

pub use parse::SchemaError;

use std::collections::HashMap;

use crate::dialect::Dialect;
use crate::model::name::{Name, TableName};
use crate::model::ty::Type;

/// A set of tables keyed by normalized name, qualified with the schema when the DDL names one
/// (`sales.emp`). A bare name resolves through `bare` to the one table carrying it; two tables
/// with the same bare name in different schemas make it ambiguous, and only a qualified reference
/// reaches either (Calcite's rule-test catalog has `sales.emp` and `scott.emp` of different width).
#[derive(Debug, Clone, Default)]
pub struct Schema {
    tables: HashMap<String, Table>,
    bare: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone)]
pub struct Table {
    pub name: TableName,
    pub columns: Vec<Column>,
    /// Normalized names of the primary-key columns.
    pub primary_key: Vec<String>,
    pub indexes: Vec<Index>,
    pub foreign_keys: Vec<ForeignKey>,
}

#[derive(Debug, Clone)]
pub struct Column {
    pub name: Name,
    pub ty: Type,
    pub nullable: bool,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub struct Index {
    pub name: Option<Name>,
    /// Normalized key column names, in index order.
    pub columns: Vec<String>,
    /// Normalized non-key `INCLUDE` columns — payload that makes the index covering.
    pub include: Vec<String>,
    pub unique: bool,
}

#[derive(Debug, Clone)]
pub struct ForeignKey {
    pub columns: Vec<String>,
    pub ref_table: String,
    pub ref_columns: Vec<String>,
}

impl Schema {
    /// Parse `CREATE TABLE` / `CREATE INDEX` DDL (Postgres dialect) into a schema. Other
    /// statements are ignored; a DDL string that fails to parse yields [`SchemaError`].
    pub fn from_ddl(sql: &str) -> Result<Schema, SchemaError> {
        Self::from_ddl_with(sql, Dialect::Postgres)
    }

    /// Like [`Self::from_ddl`] but parsing under `dialect` — so MySQL DDL (backticks,
    /// `AUTO_INCREMENT`, …) is read correctly, e.g. by the MySQL verification harness.
    pub fn from_ddl_with(sql: &str, dialect: Dialect) -> Result<Schema, SchemaError> {
        parse::from_ddl(sql, dialect)
    }

    /// Build a schema directly from `tables`, keyed by normalized name like the parsed schema.
    /// Used for the inferred, index-less schema that drives schema-less advice.
    pub fn from_tables(tables: Vec<Table>) -> Schema {
        let mut schema = Schema::default();
        for t in tables {
            schema.insert(t);
        }
        schema
    }

    pub(crate) fn insert(&mut self, table: Table) {
        let key = Self::key(&table.name);
        let bare = table.name.name.normalized();
        match self.bare.get(&bare) {
            None => {
                self.bare.insert(bare, Some(key.clone()));
            }
            Some(Some(existing)) if *existing != key => {
                self.bare.insert(bare, None);
            }
            _ => {}
        }
        self.tables.insert(key, table);
    }

    fn key(name: &TableName) -> String {
        name.key()
    }

    /// Look up a table by a key [`TableName::key`] produced: qualified first, then bare.
    pub fn table_key(&self, key: &str) -> Option<&Table> {
        self.tables
            .get(key)
            .or_else(|| self.table(&Name::new(key, false)))
    }

    /// Look up a table by bare name (Postgres folding applies); `None` when two schemas carry it.
    pub fn table(&self, name: &Name) -> Option<&Table> {
        let key = self.bare.get(&name.normalized())?.as_ref()?;
        self.tables.get(key)
    }

    pub(crate) fn table_mut(&mut self, name: &Name) -> Option<&mut Table> {
        let key = self.bare.get(&name.normalized())?.clone()?;
        self.tables.get_mut(&key)
    }

    /// Look up a table by its reference: the qualified table when the reference names a schema
    /// the DDL declared, else the bare name.
    pub fn table_ref(&self, name: &TableName) -> Option<&Table> {
        if name.schema.is_some() {
            if let Some(t) = self.tables.get(&Self::key(name)) {
                return Some(t);
            }
        }
        self.table(&name.name)
    }

    /// Original-case table names, for "did you mean" suggestions.
    pub fn table_names(&self) -> Vec<String> {
        self.tables
            .values()
            .map(|t| t.name.name.text.clone())
            .collect()
    }

    pub fn len(&self) -> usize {
        self.tables.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tables.is_empty()
    }
}

impl Table {
    /// Find a column by name (Postgres folding applies).
    pub fn column(&self, name: &Name) -> Option<&Column> {
        self.columns.iter().find(|c| c.name.matches(name))
    }
}

/// Caller-supplied table statistics: an estimated row count per table, so index advice can reason
/// about volume instead of hedging ("if the table is large"). Keyed by normalized table name.
#[derive(Debug, Clone, Default)]
pub struct Stats {
    rows: HashMap<String, u64>,
}

impl Stats {
    /// Parse a JSON object mapping table name to an estimated row count, e.g. `{"orders": 2000000}`.
    pub fn from_json(json: &str) -> Result<Stats, String> {
        let raw: HashMap<String, u64> =
            serde_json::from_str(json).map_err(|e| format!("invalid stats JSON: {e}"))?;
        Ok(Stats {
            rows: raw
                .into_iter()
                .map(|(k, v)| (Name::new(k, false).normalized(), v))
                .collect(),
        })
    }

    /// The estimated row count for `table`, if supplied.
    pub fn rows(&self, table: &Name) -> Option<u64> {
        self.rows.get(&table.normalized()).copied()
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }
}
