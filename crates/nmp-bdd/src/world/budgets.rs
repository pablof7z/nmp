//! Every bounded wait in the suite, in one place.
//!
//! These are the numbers that decide whether a scenario is a proof or a
//! flake, and each one bounds a DIFFERENT signal -- so they are deliberately
//! five constants rather than one shared "timeout", and they live together
//! so the reason each is its own number can be read side by side rather than
//! rediscovered at whichever call site happens to use it. The whole suite's
//! wall clock (the crate's `timeout 240` contract) is the sum of what is
//! written here.

use std::time::Duration;

/// Bounded wait for a positive ("eventually true") assertion.
pub const EVENTUALLY: Duration = Duration::from_secs(5);

/// Bounded wait for a negative ("never becomes true") assertion -- shorter
/// on purpose (every `never` costs its FULL window, unlike `eventually`,
/// which exits early on success; keeping this small is what keeps the whole
/// suite's wall-clock bounded -- see the crate's `timeout 240` contract).
pub const NEVER: Duration = Duration::from_millis(1200);

/// Bounded wait for a relay that just came back to be recontacted by the
/// engine's own reconnect+resubscribe (#60) -- deliberately its OWN, larger
/// budget rather than reusing `EVENTUALLY` for the whole
/// reconnect-then-observe pipeline. `nmp-transport`'s `backoff::jittered`
/// adds up to 5s of per-URL deterministic jitter on top of the small
/// `reconnect_delay_initial` this world configures, and that URL always
/// contains an OS-assigned ephemeral port -- so the jitter offset silently
/// varies run to run. Folding this wait into `EVENTUALLY` (or just raising
/// `EVENTUALLY`) would still race that jitter on an unlucky port; this
/// constant instead bounds the ACTUAL reconnect signal
/// (`ScriptedRelay::wait_contacted`) with enough headroom to cover the
/// worst-case jitter, so every step AFTER "relay comes back" runs against an
/// already-reconnected relay and never has to absorb that variance itself.
pub const RECONNECT: Duration = Duration::from_secs(8);

/// How long a relay's client-to-relay wire must stay SILENT before a
/// count-shaped assertion ("exactly one subscription", "two distinct ones",
/// "no CLOSE") is allowed to read it. Its own constant rather than a reuse of
/// `NEVER`, because it bounds something different: `NEVER` is a settle window
/// for an app-visible channel, while this one waits out the OUTBOUND HANDOFF
/// -- the gap between the engine deciding on a REQ or the CLOSE that retires
/// its predecessor and that frame actually reaching the socket the wire record
/// is read from.
///
/// THAT IS ALL IT WAITS OUT, and it used to be asked to do more. Silence on
/// the client's OUTBOUND socket says nothing about INBOUND EVENT ingestion, so
/// it cannot bound resolution: a `Derived` binding's outer filter is compiled
/// from rows arriving in the other direction, and the outbound wire is
/// genuinely quiet in the middle of that. Read as a resolution signal it
/// declared a 300-group catalog settled while the derived set still held only
/// the newest-delivered suffix of it, failing the coverage assertion in 4 runs
/// out of 6 (#1211). [`INGEST_RESOLVED`] bounds that signal; this one is the
/// socket-level complement, never a substitute for it.
pub const WIRE_QUIET: Duration = Duration::from_millis(400);

/// Ceiling on the whole quiet-down wait, so a relay whose client never stops
/// talking fails its scenario's assertion rather than hanging the suite.
pub const WIRE_SETTLE: Duration = Duration::from_secs(6);

/// Ceiling on waiting for an observation whose own wire filters are
/// downstream of rows it must ingest first (a `Derived` binding) to be told
/// by its per-source acquisition evidence that its whole subtree is proven.
///
/// Its own constant, and the largest here, because it bounds the LONGEST real
/// causal chain the suite drives: an inner REQ, a relay streaming hundreds of
/// stored events back, ingestion and re-resolution of each of them, a
/// recompiled outer filter, that filter's own REQ, and its EOSE. `EVENTUALLY`
/// bounds one app-visible transition; this bounds a whole round trip through
/// the store and back out to the wire, at a catalog size (300 groups) chosen
/// to be realistic rather than convenient. Exhausting it is reported as a
/// harness failure naming the unproven source, never absorbed -- a settle
/// that gives up quietly is exactly the defect this replaced (#1211).
pub const INGEST_RESOLVED: Duration = Duration::from_secs(20);
