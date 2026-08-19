//! **The time door: what a scenario can and cannot state about *now*.**
//!
//! This module holds no relay machinery. It holds the measured answer to the
//! question a scenario asks before it can be written at all: *can I say what
//! time this engine is running at?* The answer is "for two of the four kinds
//! of time jump, yes, through a door no application may open".
//!
//! Findings are cited against `origin/master` at the time of writing; each is
//! a file and a symbol, not a recollection.
//!
//! # 1. The reducer's clock is complete, and it is not an app door
//!
//! `nmp_runtime::EngineClock` (`crates/nmp-runtime/src/clock.rs`) is the ONE
//! wall-clock reading the engine thread makes. `EngineClock::now()` returns
//! `Timestamp::now()` while unpinned and the stated instant once pinned, and
//! it is genuinely the only such read: outside that function, `Timestamp::now()`
//! does not appear in production code anywhere in `nmp-engine`, `nmp-runtime`,
//! `nmp-store`, `nmp-nip11` or `nmp` (the one other hit,
//! `nmp-runtime/src/receipt_stream.rs:449`, is inside that file's own
//! `#[cfg(test)]` module).
//!
//! `EngineClock::set` also DELIVERS an `EngineMsg::Tick`, and commands reach
//! the engine thread over one FIFO, so a stated time is processed strictly
//! before whatever the scenario does next — no ack, no barrier, no sleep.
//! `advance` moves it forward, `set` may move it BACKWARD (documented and
//! deliberate: a device whose clock is behind is a case the write plane has
//! to survive). Its one refusal is the unix epoch, which is the unpinned
//! sentinel.
//!
//! So for the two jumps that live in the reducer, the mechanism is right:
//!
//! - **backward across a replaceable edit** — `clock.set(earlier)`.
//! - **forward across an expiry** — `clock.advance(30 days)`, and the tick it
//!   delivers is what makes the sweep actually run.
//!
//! What is wrong is the door. `Engine::clock()`
//! (`crates/nmp/src/engine.rs:450`) is `#[doc(hidden)]` and behind
//! `unstable-mechanism` — and that feature is not only a stability marker.
//! `crates/nmp/Cargo.toml` defines it as
//! `["dep:nmp-router-testkit", "nmp-engine/unstable-mechanism",
//! "nmp-runtime/unstable-mechanism"]`, so **an application that wants to say
//! what time it is links a test-fixture crate**. A reference app whose job is
//! to design the public surface cannot use that door without the surface
//! it is designing becoming untrue.
//!
//! ## The finding
//!
//! `EngineConfig` should carry the clock, the way it already carries
//! `store_path` and the way `Engine::new_with_capabilities_and_routing`
//! already takes an `AuthorRouteProvider`: an installed, app-supplied
//! decision made once at construction and fixed for the engine's life. That
//! shape is already the workspace's answer for "the app chooses the
//! algorithm, NMP compiles in none"; a clock is the same kind of thing and
//! has the same reason — an app on a device with a skewed clock, an app
//! replaying a recorded session, and a scenario stating an instant all want
//! the same seam, and today only the third has one.
//!
//! Construction-time also fixes a gap the post-construction door has: today
//! the clock can only be stated AFTER `Engine::new` returns, so nothing that
//! happens during store recovery can be given a stated time.
//!
//! # 2. The transport runs on a different clock, and nothing can jump it
//!
//! This is the larger half, and it is the one that decides whether two of the
//! four scenarios can be written at all.
//!
//! `EngineClock` governs the REDUCER. The transport does not read it:
//!
//! - **Reconnect backoff** is `Instant::now()` in
//!   `nmp-transport/src/pool/worker.rs::wait_before_reconnect` (`let deadline
//!   = Instant::now() + delay`). Advancing `EngineClock` by thirty days
//!   shortens it by nothing.
//! - **The background-gap detector** is `SystemTime::now()`, read in the
//!   worker at `pool/worker.rs` (`SuspendGapDetector::new(SystemTime::now(),
//!   SUSPEND_GAP_THRESHOLD)`). The detector itself takes an injected reading
//!   and is deterministically testable in isolation, but the worker that
//!   feeds it reads the real clock and nothing intercepts that.
//!
//! There ARE knobs for the first one: `PoolConfig::reconnect_delay_initial`
//! and `PoolConfig::reconnect_jitter_max`, both `Option<Duration>` and both
//! documented as existing precisely so an integration test need not wait out
//! the production schedule. **They are unreachable from `EngineConfig`.**
//! `crates/nmp/src/engine.rs:238` builds the pool config as
//! `PoolConfig { max_relays: config.max_relays, ..PoolConfig::default() }`,
//! and the only entry points taking a whole `PoolConfig` are
//! `Engine::from_parts*` — every one of them `#[doc(hidden)]`, behind
//! `unstable-mechanism`, and requiring a pre-built `RedbStore` (two of the
//! three also require a `nmp_router_testkit::FixtureRoutingFacts`).
//!
//! ## The consequence, stated as a number
//!
//! A reconnect scenario driving a real `nmp::Engine` through the supported
//! facade pays [`PRODUCTION_RECONNECT_FLOOR`] of real wall-clock per retry
//! and cannot jump it. That is not a tuning complaint: it is the difference
//! between a reconnect scenario being ordinary and being something nobody
//! writes.
//!
//! ## The finding
//!
//! Two doors are missing, and they are not the same door:
//!
//! 1. A **stated `now`** the whole engine reads — reducer AND transport —
//!    installed at construction. `EngineClock` is the right mechanism and the
//!    wrong scope.
//! 2. **Transport timing as configuration.** `PoolConfig`'s two reconnect
//!    overrides already exist and are already justified in their own doc
//!    comments; they are simply not projected onto `EngineConfig`. This one
//!    needs no new mechanism at all, only a field.
//!
//! Which of the two a scenario needs is decided by what it is about: a
//! scenario about an EXPIRY wants a stated instant, and a scenario about a
//! RECONNECT wants a compressed schedule. Collapsing them into one knob would
//! make the second lie — a reconnect that "took" thirty days of stated time
//! and zero real time never exercised the backoff at all.

use std::time::Duration;

/// The minimum real wall-clock a reconnect costs a scenario that drives a
/// real `nmp::Engine` through `EngineConfig`, per retry, today.
///
/// `backoff::RECONNECT_DELAY_INITIAL` (3s) plus `backoff::RECONNECT_JITTER_MAX`
/// (5s), because the jitter is a FIXED per-URL offset re-paid on every retry
/// against that URL until it connects — an unlucky ephemeral port pays it
/// every time. Both are overridable through `PoolConfig`; neither override is
/// reachable from `EngineConfig`, which is the finding in this module's doc.
///
/// A reconnect scenario budgets against this. It is a fact about NMP as it is
/// today, not a target: if the second finding above is acted on, this constant
/// stops being the floor and should be deleted rather than adjusted.
pub const PRODUCTION_RECONNECT_FLOOR: Duration = Duration::from_secs(8);
