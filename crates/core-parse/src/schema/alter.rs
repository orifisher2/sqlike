//! The keys a later `ALTER TABLE` adds. `pg_dump` writes every primary key, unique constraint and
//! foreign key this way, after the table, so a dump read without them has no keys at all
//! (`docs/phase-w0b-whole-corpus.md`, fault 2b).

use crate::model::name::{Name, TableName};
use crate::parser::ast;

use super::parse::{add_constraint, mark_primary_key};
use super::Schema;

pub(super) fn apply(schema: &mut Schema, at: &ast::AlterTable) {
    let Some(table) = schema.table_mut(&TableName::from_object_name(&at.name).name) else {
        return;
    };
    for op in &at.operations {
        match op {
            // Existing rows were never checked against a `NOT VALID` constraint, so it is no
            // premise.
            ast::AlterTableOperation::AddConstraint {
                constraint,
                not_valid: false,
            } if !deferrable(constraint) => {
                add_constraint(table, constraint);
                // Every engine that has this statement makes a primary key NOT NULL. SQLite,
                // whose primary key can admit NULL, has no `ADD CONSTRAINT`.
                mark_primary_key(table, true);
            }
            ast::AlterTableOperation::AlterColumn {
                column_name,
                op: ast::AlterColumnOperation::SetNotNull,
            } => {
                let name = Name::from_ident(column_name).normalized();
                if let Some(c) = table
                    .columns
                    .iter_mut()
                    .find(|c| c.name.normalized() == name)
                {
                    c.nullable = false;
                }
            }
            _ => {}
        }
    }
}

/// A `DEFERRABLE` constraint can be violated inside a transaction, so a query there can see rows
/// that break it.
fn deferrable(constraint: &ast::TableConstraint) -> bool {
    let characteristics = match constraint {
        ast::TableConstraint::PrimaryKey(c) => &c.characteristics,
        ast::TableConstraint::Unique(c) => &c.characteristics,
        ast::TableConstraint::ForeignKey(c) => &c.characteristics,
        _ => return false,
    };
    characteristics.is_some_and(|c| c.deferrable == Some(true))
}

#[cfg(test)]
mod tests {
    use crate::dialect::Dialect;
    use crate::model::name::Name;
    use crate::schema::{Schema, Table};

    /// The shape `pg_dump --schema-only` writes: the `\restrict` line, ownership, keys after the
    /// tables, an identity clause.
    const DUMP: &str = r"\restrict abc123
SET statement_timeout = 0;
CREATE TABLE public.u (id bigint NOT NULL, code text);
ALTER TABLE public.u OWNER TO app_owner;
CREATE TABLE public.t (id bigint NOT NULL, a int, b int, u_id bigint, c int);
ALTER TABLE public.t ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (SEQUENCE NAME public.t_id_seq);
ALTER TABLE ONLY public.t
    ADD CONSTRAINT t_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.t
    ADD CONSTRAINT t_a_key UNIQUE (a);
ALTER TABLE ONLY public.t
    ADD CONSTRAINT t_a_b_key UNIQUE (a, b);
ALTER TABLE ONLY public.u
    ADD CONSTRAINT u_pkey PRIMARY KEY (id);
ALTER TABLE ONLY public.t
    ADD CONSTRAINT fk_t_u FOREIGN KEY (u_id) REFERENCES public.u(id) ON DELETE RESTRICT;
ALTER TABLE public.t ALTER COLUMN c SET NOT NULL;
\unrestrict abc123
";

    fn table<'a>(schema: &'a Schema, name: &str) -> &'a Table {
        schema.table(&Name::new(name, false)).expect("table")
    }

    fn column(t: &Table, name: &str) -> (bool, bool) {
        let c = t
            .columns
            .iter()
            .find(|c| c.name.normalized() == name)
            .expect("column");
        (c.nullable, c.unique)
    }

    #[test]
    fn a_dump_keeps_the_keys_it_writes_after_the_table() {
        let schema = Schema::from_ddl_with(DUMP, Dialect::Postgres).expect("dump reads");
        let t = table(&schema, "t");
        assert_eq!(t.primary_key, ["id"]);
        assert_eq!(column(t, "id"), (false, true));
        assert_eq!(column(t, "a"), (true, true));
        assert_eq!(column(t, "c"), (false, false));
        let composite: Vec<_> = t.indexes.iter().map(|i| (&i.columns, i.key)).collect();
        assert_eq!(composite, [(&vec!["a".to_string(), "b".to_string()], true)]);
        let fk = &t.foreign_keys[..];
        assert_eq!(fk.len(), 1);
        assert_eq!(
            (&fk[0].columns[..], fk[0].ref_table.as_str()),
            (&["u_id".to_string()][..], "u")
        );
        assert_eq!(table(&schema, "u").primary_key, ["id"]);
    }

    #[test]
    fn a_constraint_rows_may_break_is_not_read() {
        let ddl = "CREATE TABLE u (id int PRIMARY KEY);
            CREATE TABLE t (id int NOT NULL, a int, u_id int);
            ALTER TABLE ONLY t ADD CONSTRAINT t_pkey PRIMARY KEY (id) DEFERRABLE;
            ALTER TABLE ONLY t ADD CONSTRAINT t_a_key UNIQUE (a) DEFERRABLE INITIALLY DEFERRED;
            ALTER TABLE t ADD CONSTRAINT fk FOREIGN KEY (u_id) REFERENCES u (id) NOT VALID;";
        let schema = Schema::from_ddl_with(ddl, Dialect::Postgres).expect("ddl reads");
        let t = table(&schema, "t");
        assert!(t.primary_key.is_empty());
        assert_eq!(column(t, "a"), (true, false));
        assert!(t.foreign_keys.is_empty());
    }
}
