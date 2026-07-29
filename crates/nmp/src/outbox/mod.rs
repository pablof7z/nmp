//! The write-intent/receipt plane (plan §3.4 "write outbox"). HARVEST
//! target: `crates/nmp-core/src/publish/engine/{types,mod}.rs`,
//! `kernel/publish_engine_terminals.rs` in the old repo — the per-relay
//! terminal model (`TerminalOutcome`, accepted/failed split) and the
//! enqueue≠converged discipline are re-justified there (plan §4). The
//! `Durability` class and `WriteStatus` stream are fresh framing (M0
//! amendment / ledger #6 as types) — the
//! action-ledger/correlation-id machinery from the old repo's app
//! framework is NOT carried over.
//!
//! Step D wires enqueue/route/sign-orchestration/per-relay-ack; the reducer
//! logic itself lives in `core::EngineCore` (`on_publish`/`on_signed`/
//! `on_signer_completed`/write-ack handling) — this module is the typed
//! vocabulary + the structural mechanisms (§3.4, VISION §7 ledger #6/#9).
//!
//! #115 Fable ruling (Fork 3): `Durability`/`WritePayload`/`WriteIntent`/
//! `WriteRouting` relocated to `nmp-grammar` so a
//! protocol module composing a `WriteIntent` does not gain an engine
//! dependency. `WriteStatus`/`Receipt` stay here: they reference
//! [`crate::core::ReceiptId`] and are runtime EVIDENCE an app only ever
//! reads back, never intent vocab it constructs.

use std::collections::BTreeSet;

use nostr::{EventId, PublicKey, RelayUrl, Timestamp};

use crate::core::ReceiptId;

/// The receipt STREAM (never bool/void on the durable path, ledger #9:
/// enqueue is not converged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStatus {
    Accepted,
    /// The app explicitly cancelled this accepted obligation before
    /// signature promotion. Compensation committed atomically and this
    /// terminal fact is retained for receipt reattachment.
    Cancelled,
    /// A newer accepted write won the same NIP-01 replaceable/addressable
    /// coordinate before this obligation started any wire attempt. Terminal
    /// and durably replayable; the older obligation is not retried.
    Superseded,
    /// No registered signer answers for `pubkey` -- the exact identity
    /// FROZEN at acceptance (`AcceptWrite::expected_pubkey` / an
    /// the resolved `Identity`, #47). Retained, not terminal: re-armed
    /// only by a `SignerAttached` for this exact key (never a different
    /// one, even across `set_active_account`) and re-emitted verbatim on
    /// restart replay. #47 Unit B carries the pubkey so an observer can act
    /// on (or merely display) WHICH capability the durable park is waiting
    /// for, instead of an anonymous "still waiting."
    AwaitingCapability {
        pubkey: PublicKey,
    },
    Signed(EventId),
    /// Routing has not produced a single relay yet, and `detail` says what it
    /// is waiting for ("no relay list known yet for <pubkey>").
    ///
    /// The routing sibling of [`Self::AwaitingCapability`]'s durable park:
    /// retained, NOT terminal, and re-emitted verbatim on receipt
    /// reattachment, so a route parked for a month is still visible with its
    /// reason a month later and across restarts. Nothing expires it — no
    /// TTL, no retry cap, no heuristic that decides a relay list will "never"
    /// arrive; explicit cancellation is the one abandonment door
    /// (`docs/internals/routing/preview-and-observability.md` §4).
    ///
    /// This is what replaced a real defect: a routing shortfall used to
    /// terminally [`Self::Failed`] the intent at `on_signed`, so publishing
    /// anything before the author's first relay-list fetch died permanently.
    /// "The engine had not learned enough yet" is never again a terminal
    /// verdict.
    AwaitingRoute {
        detail: String,
    },
    /// The relays this intent's strategy has resolved to SO FAR, and whether
    /// resolution can ever change its mind again.
    ///
    /// The two axes are deliberately separate. `complete` flips on settled
    /// RESOLUTION — zero remaining unknowns — never on successful delivery,
    /// which continues to stream through the per-relay facts below. So
    /// `complete: true` with every relay undelivered is an ordinary state
    /// ("we know exactly where this goes; it has not gone yet"), and so is
    /// `complete: false` with some relays already acked. Re-emitted whenever
    /// resolution changes the picture: new relays, or the `complete` flip.
    Routed {
        relays: BTreeSet<RelayUrl>,
        complete: bool,
    },
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
    /// a signer rejection, or a store-level refusal. Distinct from the
    /// per-relay `Rejected`: no `RelayUrl` exists here because none was ever
    /// reached.
    ///
    /// A routing shortfall is deliberately NOT in this class: it parks as
    /// [`Self::AwaitingRoute`] instead, because "we have not learned enough
    /// yet" is a reason to wait rather than a reason to destroy a durable,
    /// already-journaled obligation.
    Failed(String),
}

/// The only successful result of explicit write cancellation. Keeping this
/// separate from [`WriteStatus`] makes every other receipt state
/// unrepresentable as a successful cancellation result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CancelWriteOutcome {
    Cancelled,
}

/// Typed refusal from explicit pre-signature cancellation. Each terminal
/// state has its own construction path, so already-cancelled cannot be
/// represented as a refusal and accepted cannot masquerade as terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CancelWriteError {
    UnknownReceipt {
        receipt_id: ReceiptId,
    },
    AlreadySigned {
        receipt_id: ReceiptId,
        event_id: EventId,
    },
    AlreadyCompensated {
        receipt_id: ReceiptId,
    },
    AlreadySuperseded {
        receipt_id: ReceiptId,
    },
    AlreadyAbandoned {
        receipt_id: ReceiptId,
    },
    PersistenceFailed {
        receipt_id: ReceiptId,
        reason: String,
    },
    EngineClosed,
}

impl std::fmt::Display for CancelWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownReceipt { receipt_id } => {
                write!(f, "unknown receipt {}", receipt_id.0)
            }
            Self::AlreadySigned {
                receipt_id,
                event_id,
            } => write!(
                f,
                "receipt {} is already signed as {event_id}",
                receipt_id.0
            ),
            Self::AlreadyCompensated { receipt_id } => {
                write!(f, "receipt {} is already compensated", receipt_id.0)
            }
            Self::AlreadySuperseded { receipt_id } => {
                write!(
                    f,
                    "receipt {} was superseded by a newer write",
                    receipt_id.0
                )
            }
            Self::AlreadyAbandoned { receipt_id } => {
                write!(f, "receipt {} was abandoned after restart", receipt_id.0)
            }
            Self::PersistenceFailed { receipt_id, reason } => write!(
                f,
                "could not persist cancellation for receipt {}: {reason}",
                receipt_id.0
            ),
            Self::EngineClosed => write!(f, "engine already shut down"),
        }
    }
}

impl std::error::Error for CancelWriteError {}

/// What `Handle::publish` returns: an id correlating to the runtime-delivered
/// status stream — never a `bool`/`()`.
pub struct Receipt {
    pub id: ReceiptId,
}
