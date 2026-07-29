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
/// for an app-visible channel, while this one waits out RECOMPILATION.
/// Resolution is driven by ingested rows -- every demand mutation recompiles
/// the whole live demand set, with no debounce window anywhere -- so REQs keep
/// arriving for as long as demand is still resolving, and a count read
/// mid-flight would be an artifact of when it was taken rather than a fact
/// about the plan.
pub const WIRE_QUIET: Duration = Duration::from_millis(400);

/// Ceiling on the whole quiet-down wait, so a relay whose client never stops
/// talking fails its scenario's assertion rather than hanging the suite.
pub const WIRE_SETTLE: Duration = Duration::from_secs(6);
