//! The engine thread's ONE wall-clock reading.
//!
//! `EngineCore` never reads a clock. Runtime supplies wall-clock truth either
//! as an explicit `EngineMsg::Tick(now)` when deadline maintenance must run,
//! or as a cheap clock advance/current-time argument for a transition that
//! only needs to stamp or compare facts. Expiry, retry, and liveness sweeps
//! remain exclusive to `Tick`.
//!
//! That left the reducer's notion of "now" reachable only through the real
//! system clock, which is fine for production and impossible for a spec.
//! `features/writes/{event-builder,replaceable-edits}.feature` are written in
//! sentences like *"Given my device clock reads 2026-07-29T12:00:00Z"* and
//! *"And 2 seconds later ..."*, and `features/routing/cold-start-park.feature`
//! says *"And 30 days pass with nothing learned"*. None of those can be
//! asserted against a clock nobody can state.
//!
//! So the runtime reads its wall clock through this ONE value instead. In
//! production it is unpinned and every read is `Timestamp::now()`, exactly as
//! before. A harness that has to say what time it is pins it, and from then
//! on every `Tick` the runtime dispatches carries the stated instant.
//!
//! [`EngineClock::set`] also DELIVERS a `Tick`, rather than only recording a
//! number for the next reader. Two reasons, and both are what makes the
//! feature sentences mean what they say:
//!
//! - *"30 days pass"* is a claim about what the engine DID with that time
//!   (expired rows, retired routes, retried a park), and the reducer only
//!   acts on time when a `Tick` reaches it. A clock that moved silently
//!   would be a clock nobody noticed.
//! - Commands reach the engine thread over one FIFO channel, so a `Tick`
//!   posted here is processed strictly before whatever the harness does
//!   next. That is the whole ordering guarantee a step needs -- no ack, no
//!   barrier, no sleep.
//!
//! Everything here is reachable only through `nmp::mechanism`, which is
//! `#[doc(hidden)]`: none of it is an app contract.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Duration;

use nostr::Timestamp;

use super::{Cmd, EngineMsg};

/// The sentinel for "not pinned": read the real system clock. Zero is safe to
/// spend on it because a pinned engine clock of the unix epoch is not a thing
/// any caller means, and reserving it keeps the whole value one atomic load
/// on the production path.
const UNPINNED: u64 = 0;

/// Every wall-clock reading the engine thread makes, behind one shared value.
///
/// Cloned into the engine loop at spawn and kept on [`super::EngineThread`],
/// so a caller that owns the thread can state the time it is running at. See
/// the module doc for why `set` also ticks.
#[derive(Clone)]
pub struct EngineClock {
    pinned: Arc<AtomicU64>,
    /// The engine thread's own inbox, so a stated time is DELIVERED and not
    /// merely recorded. `None` on a clock that was never wired to a thread
    /// (reducer-level tests construct one), where setting the time is still
    /// meaningful for any later read but has nothing to notify.
    inbox: Option<Sender<Cmd>>,
}

impl std::fmt::Debug for EngineClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineClock")
            .field("pinned", &self.pinned.load(Ordering::Relaxed))
            .finish()
    }
}

impl EngineClock {
    /// The clock the engine thread runs on: unpinned, wired to that thread.
    pub(super) fn wired(inbox: Sender<Cmd>) -> Self {
        Self {
            pinned: Arc::new(AtomicU64::new(UNPINNED)),
            inbox: Some(inbox),
        }
    }

    /// The reading every `Tick` the runtime dispatches carries.
    #[must_use]
    pub fn now(&self) -> Timestamp {
        match self.pinned.load(Ordering::Relaxed) {
            UNPINNED => Timestamp::now(),
            secs => Timestamp::from_secs(secs),
        }
    }

    /// Change the clock value without delivering a tick. Runtime scheduling
    /// tests use this to model a command winning the channel/deadline race;
    /// production harnesses must use [`Self::set`] so stated time is acted on.
    #[cfg(any(test, feature = "bench-instrumentation"))]
    pub(super) fn pin_silently(&self, at: Timestamp) {
        self.pinned.store(at.as_secs(), Ordering::Relaxed);
    }

    /// State what time it is, and let the engine act on it.
    ///
    /// Idempotent and monotonic-agnostic: a caller may move the clock
    /// backwards (a device whose clock is behind is a case the write plane
    /// has to survive), and the reducer's own stamping rules -- not this
    /// value -- decide what that means for a write.
    pub fn set(&self, at: Timestamp) {
        let secs = at.as_secs();
        assert!(
            secs != UNPINNED,
            "nmp: the unix epoch is the engine clock's unpinned sentinel and cannot be stated"
        );
        self.pinned.store(secs, Ordering::Relaxed);
        self.tick(at);
    }

    /// Move the stated time forward. Reads the real clock first if nothing
    /// was stated yet, so "30 days pass" needs no preceding `set`.
    pub fn advance(&self, by: Duration) -> Timestamp {
        let next = Timestamp::from_secs(self.now().as_secs().saturating_add(by.as_secs()));
        self.set(next);
        next
    }

    /// Deliver the current reading without changing it -- capability #5 of
    /// issue #1013: drain whatever is due NOW instead of waiting on a
    /// wall-clock deadline to elapse on its own.
    pub fn tick_now(&self) {
        self.tick(self.now());
    }

    fn tick(&self, at: Timestamp) {
        if let Some(inbox) = &self.inbox {
            // A closed engine is not an error here: a harness may still hold
            // a clock for an engine it has already shut down, and there is
            // nothing left for the tick to reach.
            let _ = inbox.send(Cmd::Engine(EngineMsg::Tick(at)));
        }
    }
}
