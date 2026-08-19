//! **The time door, and the one that stayed shut.**
//!
//! This module holds no relay machinery. It holds the answer to the question
//! a scenario asks before it can be written at all: *can I say what time this
//! engine is running at?*
//!
//! Yes, now. `EngineConfig::clock` is a public field holding a public
//! `EngineClock`, re-exported from `nmp`, with no feature gate and no
//! `doc(hidden)`. The scenarios in `scenarios::clock` are what that made
//! writable.
//!
//! # What it replaced, because the shape of the fix is the interesting part
//!
//! The door used to be `Engine::clock()`, `#[doc(hidden)]` behind an
//! `unstable-mechanism` feature — and that feature also pulled a test-fixture
//! crate, so an application that wanted to state the time linked one. When
//! the testkits were deleted the feature went with them and took the door,
//! leaving the mechanism reachable only by abandoning `nmp::Engine` for
//! `Handle`: a different API, different nouns, no shared helpers, and nothing
//! a reference app could do without ceasing to be one.
//!
//! Two properties of the replacement are load-bearing and neither is
//! incidental:
//!
//! - **It is installed at construction, not set on a running engine.** Store
//!   recovery reads the clock, so a setter reachable only afterwards can
//!   never be true for the recovery it is meant to describe. The
//!   `stated-before-recovery` scenario is exactly this: an engine whose very
//!   first write already carries the stated instant, with nothing called on
//!   it in between.
//! - **It is a value the app owns and keeps.** `EngineClock` is cloneable, so
//!   the same clock that was true at construction still moves the engine
//!   later — which is what `backward-jump` needs, and what a
//!   construction-only argument could not give.
//!
//! # The transport runs on different clocks, deliberately
//!
//! `EngineClock` governs the REDUCER, and only that. The transport does not
//! read it:
//!
//! - **Reconnect backoff** is `Instant::now()` in
//!   `nmp-transport/src/pool/worker.rs::wait_before_reconnect`.
//! - **The background-gap detector** is `SystemTime::now()`, read in the same
//!   worker. The detector itself takes an injected reading and is
//!   deterministically testable in isolation; the worker that feeds it reads
//!   the real clock, and nothing intercepts that.
//!
//! This is not an oversight, and `scenarios::clock::transport_is_unmoved`
//! exists to keep it honest: advancing the engine clock by thirty days moves
//! the redial by nothing, and the redial then happens on the wall clock.
//!
//! A scenario about an EXPIRY wants a stated instant. A scenario about a
//! RECONNECT wants a compressed schedule. One knob for both would make the
//! second lie — a reconnect that "took" thirty days of stated time and zero
//! real time never exercised the backoff at all.
//!
//! ## The half that is still missing
//!
//! `PoolConfig::reconnect_delay_initial` and `PoolConfig::reconnect_jitter_max`
//! are `Option<Duration>` overrides documented as existing precisely so an
//! integration test need not wait out the production schedule. They are not
//! reachable from `EngineConfig`, which projects only `max_relays`
//! (`crates/nmp/src/engine.rs`: `PoolConfig { max_relays, ..default() }`).
//!
//! So the compressed-schedule door needs no new mechanism at all — only a
//! field, the same way the clock needed one. Until it exists, every scenario
//! here that waits for a redial pays [`PRODUCTION_RECONNECT_FLOOR`], and
//! three of them do.

use std::time::Duration;

/// The minimum real wall-clock a reconnect costs a scenario driving an
/// `nmp::Engine` built from an `EngineConfig`, per retry.
///
/// `backoff::RECONNECT_DELAY_INITIAL` (3s) plus `backoff::RECONNECT_JITTER_MAX`
/// (5s), because the jitter is a FIXED per-URL offset re-paid on every retry
/// against that URL until it connects — an unlucky ephemeral port pays it
/// every time.
///
/// A scenario budgets against this. It is a fact about the FACADE, not about
/// NMP: `PoolConfig` can already be told otherwise. If the missing half above
/// is filled in, this constant stops being the floor and should be deleted
/// rather than adjusted.
pub const PRODUCTION_RECONNECT_FLOOR: Duration = Duration::from_secs(8);
