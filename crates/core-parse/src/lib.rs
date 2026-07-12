//! VARQ core-parse — the pure parse/model/types layer.
//!
//! SQL → AST ([`parser`]) → stage model ([`model`]), schema DDL parsing ([`schema`]), and the
//! result types ([`result`]). No analysis engine: the rules/advise/rewrite live in `varq-core`
//! on top of this. Pure (no I/O/async/network), so it compiles to native, WASM, and tests
//! identically — the foundation the v0.3 tokenizing client rides without linking the engine.

pub mod dialect;
pub mod enrich;
/// Equivalence verdict types (v0.4) — the per-property verdict a `varq-equalizer` comparison
/// produces. Lives here, beside [`result`], because it's a *result type* the thin clients
/// deserialize; the engine that computes it stays server-only in `varq-equalizer`.
pub mod equivalence;
pub mod model;
pub mod parser;
pub mod plan;
pub mod result;
pub mod schema;
pub mod tokenize;

pub use dialect::Dialect;
