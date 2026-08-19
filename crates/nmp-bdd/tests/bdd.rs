//! The `harness = false` cucumber entry point (approach doc §2.2): parses
//! every `.feature` file under the repo-root `features/` directory and runs
//! the closed step catalog (`nmp_bdd::steps::{given,when,then}`) against
//! `NmpWorld` -- a REAL `nmp::mechanism::runtime::EngineThread` driven against
//! real in-process scripted relays, never a mocked engine.
//!
//! Two tiers:
//! - **wire tier** (default): every scenario except `@live` and legacy
//!   `@wip`/`@designed`.
//! - **live tier** (`@live`, opt-in): enabled only by `NMP_BDD_LIVE=1`;
//!   budget-capped, reuses the exact same steps against real network relays.
//!   None are staged yet; the filter below is the load-bearing gate that
//!   keeps them off by default once they exist.
//!
//! `@wip` scenarios are ALWAYS excluded: a genuine, reported gap (see each
//! such scenario's own comment) never masquerades as a passing proof (the
//! approach doc's truth-anchor rule, Appendix item 5).
//!
//! `@designed` scenarios are ALWAYS excluded for the same truth-anchor reason,
//! but they mean something different and the difference is load-bearing when
//! you read a skipped scenario months later. `@wip` is "this is built and
//! BROKEN, and here is the report". `@designed` is "this is NOT BUILT YET, and
//! this scenario is the agreed acceptance criterion for building it". Removing
//! the tag is the definition of done for the work it describes; a `@designed`
//! scenario has no step definitions yet by construction, which is precisely
//! why it must never reach the runner.
use std::path::PathBuf;

use cucumber::World as _;

#[tokio::main(flavor = "multi_thread")]
async fn main() {
    let features_dir = PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../features"));
    let run_live = std::env::var("NMP_BDD_LIVE").as_deref() == Ok("1");

    nmp_bdd::NmpWorld::cucumber()
        .max_concurrent_scenarios(1)
        // A scenario that says it reconstructs its engine gets a store that
        // survives one. Decided here, from the scenario's own sentences,
        // because the store is chosen once at start-up -- see
        // `nmp_bdd::step_crosses_a_process_boundary`.
        .before(|_feature, _rule, scenario, world| {
            let restarts = scenario
                .steps
                .iter()
                .any(|step| nmp_bdd::step_crosses_a_process_boundary(&step.value));
            Box::pin(async move {
                if restarts {
                    world.use_durable_store();
                }
            })
        })
        .filter_run_and_exit(features_dir, move |feature, _rule, scenario| {
            // A tag on the `Feature:` line covers every scenario under it;
            // gherkin does not fold feature tags into a scenario's own.
            let tagged = |name: &str| {
                feature.tags.iter().any(|t| t == name) || scenario.tags.iter().any(|t| t == name)
            };
            (!tagged("live") || run_live) && !tagged("wip") && !tagged("designed")
        })
        .await;
}
