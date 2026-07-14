//! The write-intent/receipt plane (plan §3.4 "write outbox"). HARVEST
//! target: `crates/nmp-core/src/publish/engine/{types,mod}.rs`,
//! `kernel/publish_engine_terminals.rs` in the old repo — the per-relay
//! terminal model (`TerminalOutcome`, accepted/failed split) and the
//! enqueue≠converged discipline are re-justified there (plan §4). The
//! `Durability` class, `WriteStatus` stream, and `PrivateRoute` narrow-only
//! type are fresh framing (M0 amendment / ledger #6 as types) — the
//! action-ledger/correlation-id machinery from the old repo's app
//! framework is NOT carried over.
//!
//! Step D wires enqueue/route/sign-orchestration/per-relay-ack; the reducer
//! logic itself lives in `core::EngineCore` (`on_publish`/`on_signed`/
//! `on_signer_completed`/write-ack handling) — this module is the typed
//! vocabulary + the structural mechanisms (§3.4, VISION §7 ledger #6/#9).
//!
//! #115 Fable ruling (Fork 3): `Durability`/`WritePayload`/`WriteIntent`/
//! `WriteRouting`/`NarrowOnly`/`PrivateRoute`/`HostAuthority` relocated to
//! `nmp-grammar` — a protocol module composing a `WriteIntent` (e.g.
//! `nmp-nip29::compose_group_send`) must not gain an engine dependency to
//! do so. `WriteStatus`/`Receipt`/`ReceiptSink` stay here: they reference
//! [`crate::core::ReceiptId`] and are runtime EVIDENCE an app only ever
//! reads back, never intent vocab it constructs.

use std::collections::BTreeSet;

use nostr::{EventId, RelayUrl, Timestamp};

use crate::core::ReceiptId;

/// The receipt STREAM (never bool/void on the durable path, ledger #9:
/// enqueue is not converged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStatus {
    Accepted,
    AwaitingCapability,
    Signed(EventId),
    Routed(BTreeSet<RelayUrl>),
    /// This relay lane has no in-flight EVENT attempt because its connection
    /// is unavailable. Offline time consumes no attempt ordinal.
    AwaitingRelay {
        relay: RelayUrl,
    },
    /// This relay explicitly requires AUTH before the lane may try again.
    /// AUTH-blocked time has no retry deadline and consumes no new attempt.
    AwaitingAuth {
        relay: RelayUrl,
    },
    /// The last attempt made this lane retryable at `eligible_at`. `attempt`
    /// is the persisted ordinal whose outcome established this eligibility;
    /// the next wire attempt, if one is made, receives a fresh ordinal.
    RetryEligible {
        relay: RelayUrl,
        attempt: u64,
        eligible_at: Timestamp,
    },
    /// Transport accepted a write for this persisted attempt but could not
    /// prove that it flushed. This is never a `Sent` fact. Durable delivery
    /// waits for ACK/timeout; AtMostOnce additionally becomes
    /// [`Self::OutcomeUnknown`].
    HandoffAmbiguous {
        relay: RelayUrl,
        attempt: u64,
        observed_at: Timestamp,
    },
    /// Transport proved socket write + flush for this persisted relay attempt.
    /// An ephemeral write has no outbox attempt and therefore cannot mint this
    /// durable receipt fact.
    Sent {
        relay: RelayUrl,
        attempt: u64,
        written_at: Timestamp,
    },
    Acked(RelayUrl),
    Rejected(RelayUrl, String),
    GaveUp(RelayUrl),
    /// The relay remains an owned, nonterminal delivery lane, but the
    /// durable `AttemptOutcome::Started` fact could not be committed. No
    /// wire EVENT was emitted. Recovery rediscovers the exact URL from its
    /// committed route revision; the engine's single lane scheduler owns when
    /// an in-process retry occurs.
    PersistenceBlocked(RelayUrl),
    /// The resolved relay is known in this process, but the append-only
    /// route revision itself could not be committed. No attempt or wire EVENT
    /// exists. Unlike `PersistenceBlocked`, this exact URL is not claimed to
    /// survive a crash.
    RoutePersistenceBlocked(RelayUrl),
    /// An at-most-once attempt crossed a process-loss boundary after its
    /// Started fact committed. Terminal ambiguity, never retry permission.
    OutcomeUnknown(RelayUrl),
    /// The write was a compare-and-swap whole-value replacement and the
    /// canonical local winner changed before atomic acceptance. No intent,
    /// receipt journal row, signer request, or relay write was created.
    ReplaceableConflict {
        expected: Option<EventId>,
        actual: Option<EventId>,
    },
    /// Whole-intent terminal reached BEFORE any relay was ever contacted —
    /// a signer rejection, or (ledger #6) an unroutable `PrivateNarrow`
    /// route. Distinct from the per-relay `Rejected`: no `RelayUrl` exists
    /// here because none was ever reached.
    Failed(String),
}

/// What `Handle::publish` returns: an id correlating to the status stream
/// delivered on the caller's `ReceiptSink` — never a `bool`/`()`.
pub struct Receipt {
    pub id: ReceiptId,
}

/// Sink the app-facing `Handle` registers for a `Publish`'s status stream.
pub trait ReceiptSink: Send {
    fn on_status(&self, status: WriteStatus);
}
