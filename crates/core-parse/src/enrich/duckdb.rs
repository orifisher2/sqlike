//! DuckDB-specific copy. **Deliberately empty in DD1** — every rule falls through to
//! [`super::common`], which is dialect-neutral.
//!
//! The obvious move was to seed this from `postgres.rs`, the closest dialect. Reading what is
//! actually in there talks the reader out of it: the Postgres entries are a trigram index for
//! `leading-wildcard-like`, an expression index for `non-sargable-predicate`, an array parameter
//! for `large-in-list`, and a set of "Postgres raises an error" claims for the type-mismatch
//! family. The index remedies name a structure DuckDB barely uses, and the error claims are
//! exactly where a columnar, permissively-typed engine is most likely to differ. Copying them
//! would produce confident, specific, unmeasured text — the worst output this project can emit.
//!
//! So DD1 ships the seam, not the content. DD3 fills it from measurements, and until then neutral
//! copy is the honest answer. The file exists from day one because the plan requires DuckDB's copy
//! to live somewhere no other dialect can be edited by accident.

use super::{Finding, Parts};

pub(super) fn rich(_f: &Finding) -> Option<Parts> {
    None
}
