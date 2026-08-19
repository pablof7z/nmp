//! `nmp-bdd` — the BDD acceptance layer (see `skills/nmp-dev/references/testing/`).
//! Test-only: no production crate ever depends on this one. The real entry
//! point is `tests/bdd.rs` (`harness = false`); this `src/` tree exists only
//! so that binary can `use nmp_bdd::{...}` the `World` + step catalog.


pub mod steps;

/// Does this step sentence say the scenario crosses a process boundary?
///
/// `tests/bdd.rs` asks it of every step BEFORE the scenario runs, and puts a
/// world that answers yes on a retained on-disk path. An engine-owned
/// temporary Redb directory cannot be reopened after its store is dropped, so
/// "I reconstruct the engine from the same durable store" is only a genuine
/// restart when the retained path was chosen with that sentence in
/// mind -- and the store is chosen once, at start-up, before any `When`
/// exists to ask. #974 answered this with a `Given` that set a flag; reading
/// the scenario's own words means a `.feature` never has to name the harness's
/// storage engine to get the behaviour it already asked for in English.
#[must_use]
pub fn step_crosses_a_process_boundary(step: &str) -> bool {
    step.contains("reconstruct the engine") || step.contains("the process stops")
}
pub mod world;

pub use world::NmpWorld;
