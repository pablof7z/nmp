//! The engine thread's ONE wall-clock reading.
//!
//! `EngineCore` never reads a clock. Runtime supplies wall-clock truth either
//! as an explicit `EngineMsg::Tick(now)` when deadline maintenance must run,
//! or as a cheap clock advance/current-time argument for a transition that
//! only needs to stamp or compare facts. Expiry, retry, and liveness sweeps
//! remain exclusive to `Tick`.
//!
//! That left the reducer's notion of "now" reachable only through the real
//! system clock. So the runtime reads its wall clock through this ONE value
//! instead. Unpinned -- which is what an application that never mentions a
//! clock gets -- every read is `Timestamp::now()`, exactly as before. An
//! application that has to state what time it is pins it, and from then on
//! every `Tick` the runtime dispatches carries the stated instant.
//!
//! # Why an application owns this value, rather than borrowing it back
//!
//! The clock is handed to [`EngineThread`](crate::EngineThread) at
//! construction, through
//! [`RuntimeConfig::clock`](crate::RuntimeConfig::clock) -- the same shape as
//! the `AuthorRouteProvider` beside it, and for the same reason: it is a
//! decision the host makes, not one NMP compiles in.
//!
//! Construction time is not a stylistic choice. A clock reachable only AFTER
//! the engine is running cannot state the time that store recovery runs at,
//! because recovery has already happened by the time the caller has anything
//! to call. `engine_loop` gives the reducer `clock.now()` immediately before
//! `recover_on_boot`, so a clock stated before construction is the clock
//! recovery sees.
//!
//! The three cases that want this are the same case: an app on a device whose
//! clock is skewed, an app replaying a recorded session, and a scenario that
//! says *"30 days pass"* all need to say what instant the reducer is running
//! at, and none of them is a test-only concern.
//!
//! # Why [`EngineClock::set`] also delivers a `Tick`
//!
//! It DELIVERS a `Tick`, rather than only recording a number for the next
//! reader. Two reasons:
//!
//! - *"30 days pass"* is a claim about what the engine DID with that time
//!   (expired rows, retired routes, retried a park), and the reducer only
//!   acts on time when a `Tick` reaches it. A clock that moved silently
//!   would be a clock nobody noticed.
//! - Commands reach the engine thread over one FIFO channel, so a `Tick`
//!   posted here is processed strictly before whatever the caller does
//!   next. That is the whole ordering guarantee a caller needs -- no ack, no
//!   barrier, no sleep.
//!
//! # What this clock is NOT
//!
//! It is the REDUCER's clock. The transport runs on its own: reconnect
//! backoff is `Instant::now()` and the background-gap detector is
//! `SystemTime::now()`, both read in `nmp-transport`'s pool worker. Advancing
//! this clock by thirty days shortens neither, and that is deliberate --
//! a question about an expiry wants a stated INSTANT, a question about a
//! reconnect wants a compressed SCHEDULE, and one knob answering both would
//! make the second one lie.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
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
/// Construct one, state a time on it if you have one to state, and hand it to
/// the engine through [`RuntimeConfig::clock`](crate::RuntimeConfig::clock);
/// the clone you keep drives the engine you gave it to. A default-constructed
/// clock is unpinned, which is the production path: `Timestamp::now()` at
/// every read, byte for byte what the runtime did before this value existed.
///
/// See the module doc for why `set` also ticks and for what this clock does
/// not govern.
#[derive(Clone, Default)]
pub struct EngineClock {
    pinned: Arc<AtomicU64>,
    /// The inboxes of the engine threads this clock was installed on, so a
    /// stated time is DELIVERED and not merely recorded. Empty until the
    /// clock is handed to an engine, where setting the time is still
    /// meaningful for any later read but has nothing to notify.
    ///
    /// A `Vec` because one clock may legitimately be installed on more than
    /// one engine -- they already share the pinned reading through the
    /// `Arc` above, so anything less than telling all of them would make a
    /// shared clock a clock only one engine noticed. Senders whose engine has
    /// exited are dropped on the first tick that finds them closed.
    inboxes: Arc<Mutex<Vec<Sender<Cmd>>>>,
}

impl std::fmt::Debug for EngineClock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EngineClock")
            .field("pinned", &self.pinned.load(Ordering::Relaxed))
            .finish()
    }
}

impl EngineClock {
    /// An unpinned clock: every read is the real system clock until something
    /// states otherwise.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Install this clock on an engine thread's inbox, so a time stated on it
    /// from now on is delivered to that engine and not merely recorded.
    pub(super) fn install(&self, inbox: Sender<Cmd>) {
        self.lock_inboxes().push(inbox);
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
    /// benchmarks use this to model a command winning the channel/deadline
    /// race; every other caller uses [`Self::set`] so stated time is acted on.
    #[cfg(feature = "bench-instrumentation")]
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

    /// Deliver the current reading without changing it: drain whatever is due
    /// NOW instead of waiting on a wall-clock deadline to elapse on its own.
    pub fn tick_now(&self) {
        self.tick(self.now());
    }

    fn tick(&self, at: Timestamp) {
        // A closed engine is not an error here: a caller may still hold a
        // clock for an engine it has already shut down, and there is nothing
        // left for the tick to reach. Dropping that sender is the only
        // cleanup this value needs.
        self.lock_inboxes()
            .retain(|inbox| inbox.send(Cmd::Engine(EngineMsg::Tick(at))).is_ok());
    }

    /// Poisoning carries no meaning for this value -- nothing here can leave
    /// a half-written `Vec` behind, since the only mutations are a push and a
    /// retain over infallible sends.
    fn lock_inboxes(&self) -> std::sync::MutexGuard<'_, Vec<Sender<Cmd>>> {
        self.inboxes
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}
