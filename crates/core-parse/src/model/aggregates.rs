//! Which function names are aggregates on each engine, measured on the engines themselves by
//! `crates/verify/tests/aggregate_lists.rs` (`docs/phase-ag1-aggregates-by-engine.md`): over a
//! table of two rows, an aggregate returns one row, a scalar two, and an absent function errors.
//! Postgres's list is its own catalog's (`pg_aggregate.aggkind = 'n'`). The lists are the engines'
//! documented built-ins, public knowledge.
//!
//! A name read as a scalar when it is an aggregate let a pass fold it into a grouped derived table
//! and equate one row with one row per group (#1035, then 28 more names). A name read as an
//! aggregate when it is a scalar is unsound the other way: over no rows an ungrouped aggregate
//! still returns one. So the kind is the engine's, decided where the dialect is known.

use crate::dialect::Dialect;

use super::expr::FnKind;

const POSTGRES: &[&str] = &[
    "any_value",
    "array_agg",
    "avg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bool_and",
    "bool_or",
    "corr",
    "count",
    "covar_pop",
    "covar_samp",
    "every",
    "json_agg",
    "json_agg_strict",
    "json_arrayagg",
    "json_object_agg",
    "json_object_agg_strict",
    "json_object_agg_unique",
    "json_object_agg_unique_strict",
    "jsonb_agg",
    "jsonb_agg_strict",
    "jsonb_object_agg",
    "jsonb_object_agg_strict",
    "jsonb_object_agg_unique",
    "jsonb_object_agg_unique_strict",
    "max",
    "min",
    "range_agg",
    "range_intersect_agg",
    "regr_avgx",
    "regr_avgy",
    "regr_count",
    "regr_intercept",
    "regr_r2",
    "regr_slope",
    "regr_sxx",
    "regr_sxy",
    "regr_syy",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "string_agg",
    "sum",
    "var_pop",
    "var_samp",
    "variance",
    "xmlagg",
];

/// MySQL and MariaDB measured the same list. MySQL's `any_value` is not on it: it runs, and
/// returns a row per input row (see [`SCALAR_ON`]).
const MYSQL: &[&str] = &[
    "avg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "count",
    "group_concat",
    "json_arrayagg",
    "json_objectagg",
    "max",
    "min",
    "std",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "sum",
    "var_pop",
    "var_samp",
    "variance",
];

const MSSQL: &[&str] = &[
    "approx_count_distinct",
    "avg",
    "checksum_agg",
    "count",
    "count_big",
    "max",
    "min",
    "stdev",
    "stdevp",
    "string_agg",
    "sum",
    "var",
    "varp",
];

/// `max` and `min` here are aggregates with one argument only (see [`kind`]).
const SQLITE: &[&str] = &[
    "avg",
    "count",
    "group_concat",
    "json_group_array",
    "json_group_object",
    "jsonb_group_array",
    "jsonb_group_object",
    "max",
    "min",
    "string_agg",
    "sum",
    "total",
];

const DUCKDB: &[&str] = &[
    "any_value",
    "approx_count_distinct",
    "approx_quantile",
    "approx_top_k",
    "arbitrary",
    "arg_max",
    "arg_max_null",
    "arg_max_nulls_last",
    "arg_min",
    "arg_min_null",
    "arg_min_nulls_last",
    "argmax",
    "argmin",
    "array_agg",
    "avg",
    "bit_and",
    "bit_or",
    "bit_xor",
    "bitstring_agg",
    "bool_and",
    "bool_or",
    "corr",
    "count",
    "count_if",
    "count_star",
    "countif",
    "covar_pop",
    "covar_samp",
    "entropy",
    "favg",
    "first",
    "fsum",
    "group_concat",
    "histogram",
    "kahan_sum",
    "kurtosis",
    "kurtosis_pop",
    "last",
    "list",
    "listagg",
    "mad",
    "max",
    "max_by",
    "mean",
    "median",
    "min",
    "min_by",
    "mode",
    "product",
    "quantile",
    "quantile_cont",
    "quantile_disc",
    "regr_avgx",
    "regr_avgy",
    "regr_count",
    "regr_intercept",
    "regr_r2",
    "regr_slope",
    "regr_sxx",
    "regr_sxy",
    "regr_syy",
    "reservoir_quantile",
    "sem",
    "skewness",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "string_agg",
    "sum",
    "sum_no_overflow",
    "sumkahan",
    "var_pop",
    "var_samp",
    "variance",
    "variant_group_object",
];

/// Names an engine runs as a scalar although another engine aggregates them.
const SCALAR_ON: &[(Dialect, &str)] = &[(Dialect::Mysql, "any_value")];

/// The aggregate names of one engine.
pub fn aggregates(dialect: Dialect) -> &'static [&'static str] {
    match dialect {
        Dialect::Postgres => POSTGRES,
        Dialect::Mysql | Dialect::Mariadb => MYSQL,
        Dialect::Mssql => MSSQL,
        Dialect::Sqlite => SQLITE,
        Dialect::Duckdb => DUCKDB,
    }
}

/// What a call of `name` (normalized) with `arity` arguments is on `dialect`. A name the engine
/// does not have is `Unknown`: on that engine the query does not run, or it is a user-defined
/// function, which could be either.
pub fn kind(name: &str, arity: usize, dialect: Dialect) -> FnKind {
    // SQLite's `max(a, b)` and `min(a, b)` are scalars: measured, one row for `max(a)` over two
    // rows and two for `max(a, a)`.
    if dialect == Dialect::Sqlite && arity >= 2 && matches!(name, "max" | "min") {
        return FnKind::Scalar;
    }
    if aggregates(dialect).contains(&name) {
        FnKind::Aggregate
    } else if SCALAR_ON.contains(&(dialect, name)) {
        FnKind::Scalar
    } else {
        FnKind::Unknown
    }
}

/// Whether some engine aggregates `name`. What an `Unknown` call is taken to be: on its own engine
/// it does not run or is user-defined, and reading an engine's aggregate as a scalar is the
/// defect this module exists for.
pub fn aggregate_somewhere(name: &str) -> bool {
    [POSTGRES, MYSQL, MSSQL, SQLITE, DUCKDB]
        .iter()
        .any(|list| list.contains(&name))
}
