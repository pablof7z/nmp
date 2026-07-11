//! The `harness = false` cucumber entry point (approach doc §2.2): parses
//! every `.feature` file under the repo-root `features/` directory and runs
//! the closed step catalog (`nmp_bdd::steps::{given,when,then}`) against
//! `NmpWorld` -- a REAL `nmp_engine::runtime::EngineThread` driven against
//! real in-process scripted relays, never a mocked engine.
//!
//! Two tiers (approach doc §2.2):
//! - **wire tier** (default, CI, every push): every scenario except `@live`
//!   and `@wip`.
//! - **live tier** (`@live`, opt-in): enabled only by `NMP_BDD_LIVE=1` --
//!   NOT exercised by this repo's CI; budget-capped, reuses the exact same
//!   steps against real network relays. None are staged yet (§2.2's
//!   handful is future work); the filter below is the load-bearing gate
//!   that keeps them off by default once they exist.
//!
//! `@wip` scenarios are ALWAYS excluded: a genuine, reported gap (see each
//! such scenario's own comment) never masquerades as a passing proof (the
//! approach doc's truth-anchor rule, Appendix item 5).
use std::path::PathBuf;

use cucumber::World as _;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let features_dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../features"));
    let run_live = std::env::var("NMP_BDD_LIVE").as_deref() == Ok("1");

    nmp_bdd::NmpWorld::cucumber()
        .max_concurrent_scenarios(1)
        .filter_run_and_exit(features_dir, move |_feature, _rule, scenario| {
            let is_live = scenario.tags.iter().any(|t| t == "live");
            let is_wip = scenario.tags.iter().any(|t| t == "wip");
            (!is_live || run_live) && !is_wip
        })
        .await;
}
