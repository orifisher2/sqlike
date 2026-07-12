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
    let statements = parser::parse(sql, dialect).map_err(|e| SchemaError::Parse(e.to_string()))?;
    let mut schema = Schema::default();

    // Tables first, then indexes (which attach to already-built tables).
    for stmt in &statements {
        if let ast::Statement::CreateTable(ct) = stmt {
            let table = build_table(ct);
            schema.tables.insert(table.name.name.normalized(), table);
        }
    }
    for stmt in &statements {
        if let ast::Statement::CreateIndex(ci) = stmt {
            apply_index(&mut schema, ci);
        }
    }
    Ok(schema)
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

    for c in &ct.constraints {
        match c {
            ast::TableConstraint::PrimaryKey(pk) => {
                for ic in &pk.columns {
                    if let Some(n) = index_column_name(ic) {
                        primary_key.push(n);
                    }
                }
            }
            ast::TableConstraint::Unique(u) => {
                for ic in &u.columns {
                    if let Some(n) = index_column_name(ic) {
                        mark_unique(&mut columns, &n);
                    }
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

    // Primary-key columns are NOT NULL and unique.
    for pk in &primary_key {
        if let Some(c) = columns.iter_mut().find(|c| &c.name.normalized() == pk) {
            c.nullable = false;
            c.unique = true;
        }
    }

    Table {
        name: TableName::from_object_name(&ct.name),
        columns,
        primary_key,
        indexes: Vec::new(),
        foreign_keys,
    }
}

fn apply_index(schema: &mut Schema, ci: &ast::CreateIndex) {
    let table_key = TableName::from_object_name(&ci.table_name)
        .name
        .normalized();
    let columns: Vec<String> = ci.columns.iter().filter_map(index_column_name).collect();
    let index = Index {
        name: ci.name.as_ref().map(crate::model::name::object_name_last),
        columns,
        include: ci.include.iter().map(norm_ident).collect(),
        unique: ci.unique,
    };
    if let Some(t) = schema.tables.get_mut(&table_key) {
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
