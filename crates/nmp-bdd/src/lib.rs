//! `nmp-bdd` — the BDD acceptance layer (`docs/bdd/000-bdd-approach.md`).
//! Test-only: no production crate ever depends on this one. The real entry
//! point is `tests/bdd.rs` (`harness = false`); this `src/` tree exists only
//! so that binary can `use nmp_bdd::{...}` the `World` + step catalog.

pub mod reference_fixtures;
pub mod relays;
pub mod steps;
pub mod world;

pub use world::NmpWorld;
