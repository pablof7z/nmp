//! **The time door: there is no longer one.**
//!
//! This module holds no relay machinery. It holds the measured answer to the
//! question a scenario asks before it can be written at all: *can I say what
//! time this engine is running at?*
//!
//! The answer used to be "yes, through a door no application may open". It is
//! now "no".
//!
//! # What changed
//!
//! `Engine::clock()` was `#[doc(hidden)]` behind an `unstable-mechanism`
//! feature. That feature was defined as
//! `["dep:nmp-router-testkit", "nmp-engine/unstable-mechanism", ...]`, and it
//! went when the testkit crates went. `crates/nmp/Cargo.toml` now declares
//! exactly three features -- `default`, `bench-instrumentation`,
//! `test-instrumentation` -- and `crates/nmp/src/` contains no clock door of
//! any kind; the single surviving occurrence of the word is an unrelated
//! sentence in `config.rs` about `max_publish_attempts` counting observations
//! rather than wall-clock.
//!
//! This is not a grep-only claim. `nmp-relay-lab`'s own test suite named
//! `nmp = { features = ["unstable-mechanism"] }` and cargo refused the build:
//! *"package `nmp-relay-lab` depends on `nmp` with feature
//! `unstable-mechanism` but `nmp` does not have that feature."*
//!
//! # The mechanism is intact, and further out of reach than before
//!
//! `nmp_runtime::EngineClock` still exists and is still complete for what it
//! covers. `EngineClock::now()` is the ONE wall-clock reading the engine
//! thread makes; `set` delivers an `EngineMsg::Tick` over the same FIFO the
//! caller's next command uses, so a stated time is acted on with no barrier
//! and no sleep; `advance` moves forward and `set` may move BACKWARD, which
//! is documented as deliberate because a device whose clock is behind is a
//! case the write plane has to survive.
//!
//! `EngineThread::clock()` is `pub` and carries no `cfg` at all, and
//! `EngineThread::spawn(RedbStore, usize, PoolConfig)` and
//! `RedbStore::temporary()` are likewise `pub` and ungated. So the mechanism
//! is reachable -- by a caller that abandons `nmp::Engine` and assembles an
//! engine out of `nmp-runtime`, `nmp-store` and `nmp-transport` by hand.
//!
//! That is worse than the old situation, not better. Before, stating the time
//! cost a feature flag on the product facade. Now it costs giving up the
//! product facade entirely: a scenario that wants a clock must drive `Handle`
//! instead of `Engine`, which is a different API with different nouns
//! (`RowsReceiver` rather than `Frame`, no `observe`, no `Subscription`), so
//! nothing written against the app's own surface can be reused. A reference
//! app cannot do it at all without ceasing to be a reference app.
//!
//! ## The finding
//!
//! `EngineConfig` should carry the clock, the way it already carries
//! `store_path` and the way `Engine::new_with_capabilities_and_routing`
//! already takes an `AuthorRouteProvider`: an app-supplied decision made once
//! at construction and fixed for the engine's life. That shape is already the
//! workspace's answer for "the app chooses, NMP compiles in none".
//!
//! Construction-time also closes a gap the deleted door never had a chance
//! to: nothing that happens during store recovery can be given a stated time
//! by a setter that only exists afterwards.
//!
//! # The transport runs on a different clock, and that has NOT changed
//!
//! `EngineClock` governs the REDUCER. The transport does not read it:
//!
//! - **Reconnect backoff** is `Instant::now()` in
//!   `nmp-transport/src/pool/worker.rs::wait_before_reconnect`
//!   (`let deadline = Instant::now() + delay`).
//! - **The background-gap detector** is `SystemTime::now()`, read in the same
//!   worker (`SuspendGapDetector::new(SystemTime::now(), SUSPEND_GAP_THRESHOLD)`).
//!   The detector itself takes an injected reading and is deterministically
//!   testable in isolation; the worker that feeds it reads the real clock and
//!   nothing intercepts that.
//!
//! So even a caller willing to assemble an engine by hand cannot make thirty
//! days pass for a backoff. What it CAN now do is shorten the schedule:
//! `PoolConfig::reconnect_delay_initial` and `PoolConfig::reconnect_jitter_max`
//! are `Option<Duration>` overrides documented as existing precisely so an
//! integration test need not wait out the production schedule, and
//! `EngineThread::spawn` takes a whole `PoolConfig`. They remain unreachable
//! from `EngineConfig`, which projects only `max_relays`
//! (`crates/nmp/src/engine.rs`: `PoolConfig { max_relays, ..default() }`).
//!
//! ## The second finding
//!
//! Two doors are missing, and they are not the same door. A scenario about an
//! EXPIRY wants a stated instant; a scenario about a RECONNECT wants a
//! compressed schedule. One knob for both would make the second lie -- a
//! reconnect that "took" thirty days of stated time and zero real time never
//! exercised the backoff at all.
//!
//! The second is the cheaper of the two and needs no new mechanism: project
//! the two `PoolConfig` overrides onto `EngineConfig`.

use std::time::Duration;

/// The minimum real wall-clock a reconnect costs a scenario driving an
/// `nmp::Engine` built from an `EngineConfig`, per retry.
///
/// `backoff::RECONNECT_DELAY_INITIAL` (3s) plus `backoff::RECONNECT_JITTER_MAX`
/// (5s), because the jitter is a FIXED per-URL offset re-paid on every retry
/// against that URL until it connects -- an unlucky ephemeral port pays it
/// every time.
///
/// A scenario budgets against this. It is a fact about the FACADE, not about
/// NMP: `PoolConfig` can already be told otherwise, and an engine assembled
/// from `EngineThread::spawn` pays whatever it asks for. If the second finding
/// above is acted on, this constant stops being the floor and should be
/// deleted rather than adjusted.
pub const PRODUCTION_RECONNECT_FLOOR: Duration = Duration::from_secs(8);
