//! The PURE synchronous reducer (plan §2 position 1, §3.4). `EngineCore`
//! owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-outbox
//! state, and the coverage-attribution bookkeeping (`attribution.rs`,
//! `evidence.rs`). Its entire surface is:
//!
//! ```ignore
//! impl<S: EventStore> EngineCore<S> {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//!     pub fn next_deadline(&self) -> Option<nostr::Timestamp>;
//! }
//! ```
//!
//! `EngineCore` does NO I/O, spawns no threads, touches no socket, imposes
//! no runtime — this is the seam that preserves M1/M2's headless property:
//! the whole engine's logic is testable by feeding `EngineMsg`s and
//! asserting `Effect`s, with zero network (plan §5 tier A).
//!
//! Coverage attribution implements
//! `docs/consults/2026-07-11-fable-coverage-attribution.md` (the ruling)
//! EXACTLY: send-time snapshots + the FIFO intersection rule live in
//! [`attribution`]; the per-query, per-source acquisition evidence (`rows +
//! compact facts, never a collapsed global verdict` —
//! `docs/design/scoped-evidence-49-12-plan.md`, folding #12 into #49) lives
//! in [`evidence`]. Both are engine-owned — the store (`nmp-store`) only
//! stores whatever interval it is handed.

mod admission;
mod attribution;
#[cfg(test)]
mod auth_core_headless;
mod diagnostics;
mod evidence;
mod history;

use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::rc::Rc;

#[cfg(test)]
use std::cell::Cell;

use nostr::{
    filter::MatchEventOptions, Event as SignedEvent, EventBuilder, EventId, PublicKey,
    RelayMessage, RelayUrl, Timestamp, UnsignedEvent,
};

use nmp_grammar::{
    AccessContext, Binding, CacheMode, ConcreteFilter, ContextualAtom, DescriptorHash, Durability,
    Filter, HostAuthority, NarrowOnly, PrivateRoute, RelaySessionKey, RoutingEvidence,
    SourceAuthority, WriteIntent, WritePayload, WriteRouting,
};
use nmp_resolver::{
    CommittedMutationResult, CommittedRowChanges, Engine as ResolverEngine, HandleId, LiveQuery,
    LocalAcceptResult, QueryHandle,
};
use nmp_router::{
    DiscoveryKinds, Lane, LanedRelay, PubkeyHex, RelayDirectory, RelayPlan, Router, RuleRegistry,
    SubId, WireDelta, WireOp, WireReq,
};
use nmp_signer::SignerError;
use nmp_store::{
    sentinel_signature, AcceptOutcome, AcceptWrite, AttemptHandoffDetail, AttemptOutcome,
    CloseIntentOutcome, CompensateOutcome, CoverageKey, DeadlineKind, EventStore, HandoffEvidence,
    InFlightPhase, IntentId, IntentSigState, LaneKey, LaneState, PersistenceError,
    PostHandoffState, PromoteOutcome, ReceiptState, RecoveredLane, RelayObserved, TransientCause,
    WriteDurability,
};
use nmp_transport::{
    AttemptCorrelation, DisconnectReason, HandoffResult, RelayFrame,
    RelayHandle as TransportRelayHandle, RelayHealth,
};

use crate::negentropy::{NegStep, ProbedRelay, Prober, Reconciler};
use crate::outbox::{ReceiptSink, WriteStatus};
use crate::relay_information::RelayInformationCapabilityEvidence;

/// The liveness deadline (plan §4/harvest `nmp-nip77`) past which an open
/// negentropy session with no reply is abandoned in favor of a plain REQ
/// (never left to hang forever, and never silently re-tried as negentropy
/// again on the same generation -- `tick`'s own staleness sweep is the only
/// caller of this constant).
const NEG_LIVENESS_DEADLINE_SECS: u64 = 30;

const RETRY_INITIAL_SECS: u64 = 3;
const RETRY_MAX_SECS: u64 = 300;
const RETRY_JITTER_MAX_SECS: u64 = 5;
const ACK_TIMEOUT_SECS: u64 = 30;
/// NIP-42 permits an authentication event at most ten minutes from relay
/// receipt. We spend that future window as a checked per-live-session nonce
/// when repeated identical challenges arrive inside one reducer second.
const AUTH_MAX_FUTURE_SECS: u64 = 600;
/// Never minted by `mint_auth_sequence`; owned exclusively by the
/// counter-exhausted fallback `AuthEpoch` (phase `Error`) so sentinel and
/// real epochs are distinct BY VALUE, not merely by phase.
const AUTH_SEQUENCE_SENTINEL: u64 = u64::MAX;
const MAX_GLOBAL_ATTEMPTS: usize = 32;
const DEADLINE_READ_BATCH: usize = 1_024;

fn retry_delay_secs(key: &LaneKey, ordinal: u64) -> u64 {
    let exponent = ordinal.saturating_sub(1).min(63) as u32;
    let base = RETRY_INITIAL_SECS
        .checked_shl(exponent)
        .unwrap_or(u64::MAX)
        .min(RETRY_MAX_SECS);

    // FNV-1a is used as a deliberately tiny, fully specified stable hash.
    // Jitter is policy spreading, not a security boundary; unlike
    // DefaultHasher this remains identical across processes and releases.
    let mut hash = 0xcbf29ce484222325u64;
    for byte in key
        .intent_id
        .0
        .to_be_bytes()
        .into_iter()
        .chain(key.relay.as_str().as_bytes().iter().copied())
        .chain(ordinal.to_be_bytes())
    {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    base.saturating_add(hash % RETRY_JITTER_MAX_SECS)
}

#[cfg(test)]
mod relay_session_key_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::{coverage_key, MemoryStore};
    use nostr::{Keys, SubscriptionId};

    fn relay() -> RelayUrl {
        RelayUrl::parse("wss://session.example.com").unwrap()
    }

    #[test]
    fn wrong_context_eose_cannot_consume_or_credit_another_session() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let access_a = AccessContext::Nip42(a);
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atom = ContextualAtom {
            filter: filter.clone(),
            source: SourceAuthority::Public,
            access: access_a,
            routing_evidence: BTreeSet::new(),
        };
        let key = coverage_key(&atom);
        let sub_id = SubId::for_wire(relay.clone(), &filter, &SourceAuthority::Public, access_a);
        let session_a = RelaySessionKey::new(relay.clone(), access_a);
        let session_b = RelaySessionKey::new(relay, AccessContext::Nip42(b));
        let mut attribution = AttributionState::new();
        attribution.observe_demand([&atom]);
        attribution.record_send(&session_a, &sub_id, &filter, BTreeSet::from([key]));
        let wire_id = wire_sub_id_string(&sub_id);

        assert!(attribution
            .attribute_eose(&session_b, &wire_id, Timestamp::from(10u64))
            .is_empty());
        assert_eq!(
            attribution
                .attribute_eose(&session_a, &wire_id, Timestamp::from(10u64))
                .len(),
            1
        );
    }

    #[test]
    fn disconnecting_a_preserves_public_and_b_sessions() {
        let relay = relay();
        let a = Keys::generate().public_key();
        let b = Keys::generate().public_key();
        let public = RelaySessionKey::public(relay.clone());
        let session_a = RelaySessionKey::new(relay.clone(), AccessContext::Nip42(a));
        let session_b = RelaySessionKey::new(relay, AccessContext::Nip42(b));
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let handles = [
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            TransportRelayHandle {
                slot: 1,
                generation: 1,
            },
            TransportRelayHandle {
                slot: 2,
                generation: 1,
            },
        ];
        core.handle(EngineMsg::RelayConnected(handles[0], public.clone()));
        core.handle(EngineMsg::RelayConnected(handles[1], session_a.clone()));
        core.handle(EngineMsg::RelayConnected(handles[2], session_b.clone()));

        core.handle(EngineMsg::RelayDisconnected(
            handles[1],
            session_a.clone(),
            DisconnectReason::Closed,
        ));

        assert!(core.connected_relays.contains(&public));
        assert!(!core.connected_relays.contains(&session_a));
        assert!(core.connected_relays.contains(&session_b));
    }

    #[test]
    fn protected_neg_frames_cannot_resolve_the_public_probe_or_inherit_its_diagnostics() {
        let relay = relay();
        let public = RelaySessionKey::public(relay.clone());
        let protected = RelaySessionKey::new(
            relay.clone(),
            AccessContext::Nip42(Keys::generate().public_key()),
        );
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([1])),
            ..ConcreteFilter::default()
        };
        let atoms = BTreeSet::from([
            ContextualAtom {
                filter: filter.clone(),
                source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                access: AccessContext::Public,
                routing_evidence: BTreeSet::new(),
            },
            ContextualAtom {
                filter,
                source: SourceAuthority::Pinned(BTreeSet::from([relay.clone()])),
                access: protected.access,
                routing_evidence: BTreeSet::new(),
            },
        ]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.router
            .compile(&atoms, core.directory.as_ref(), core.cap);
        let public_handle = TransportRelayHandle {
            slot: 5,
            generation: 1,
        };
        let protected_handle = TransportRelayHandle {
            slot: 6,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(public_handle, public.clone()));
        core.handle(EngineMsg::RelayConnected(
            protected_handle,
            protected.clone(),
        ));
        let probe = core.prober.begin_probe(&relay).unwrap();
        let wire_id = wire_sub_id_string(&probe.sub_id);

        let protected_neg_msg = RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id.clone())),
            message: std::borrow::Cow::Owned("6100".to_string()),
        });
        assert!(core
            .handle(EngineMsg::RelayFrame(
                protected_handle,
                protected.clone(),
                protected_neg_msg,
            ))
            .is_empty());
        let protected_neg_err = RelayFrame::from(RelayMessage::NegErr {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id.clone())),
            message: std::borrow::Cow::Owned("blocked: unsupported".to_string()),
        });
        assert!(core
            .handle(EngineMsg::RelayFrame(
                protected_handle,
                protected.clone(),
                protected_neg_err,
            ))
            .is_empty());
        assert_eq!(
            core.prober.state(&relay),
            crate::negentropy::ProbeState::Probing
        );

        let probing = core.diagnostics_snapshot();
        let public_diagnostics = probing
            .relays
            .iter()
            .find(|entry| entry.access == AccessContext::Public)
            .unwrap();
        let protected_diagnostics = probing
            .relays
            .iter()
            .find(|entry| entry.access == protected.access)
            .unwrap();
        assert_eq!(public_diagnostics.nip77_behavior, "probing");
        assert_eq!(protected_diagnostics.nip77_behavior, "unknown");

        let public_neg_msg = RelayFrame::from(RelayMessage::NegMsg {
            subscription_id: std::borrow::Cow::Owned(SubscriptionId::new(wire_id)),
            message: std::borrow::Cow::Owned("6100".to_string()),
        });
        core.handle(EngineMsg::RelayFrame(public_handle, public, public_neg_msg));
        assert_eq!(
            core.prober.state(&relay),
            crate::negentropy::ProbeState::Supported
        );
        let resolved = core.diagnostics_snapshot();
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.access == AccessContext::Public)
                .unwrap()
                .nip77_behavior,
            "behaviorally_proven"
        );
        assert_eq!(
            resolved
                .relays
                .iter()
                .find(|entry| entry.access == protected.access)
                .unwrap()
                .nip77_behavior,
            "unknown"
        );
    }

    #[test]
    fn intentional_close_never_reopens_a_still_planned_session() {
        let relay = relay();
        let session = RelaySessionKey::public(relay.clone());
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([1])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Pinned(BTreeSet::from([relay])),
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        };
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.router
            .compile(&BTreeSet::from([atom]), core.directory.as_ref(), core.cap);
        let handle = TransportRelayHandle {
            slot: 0,
            generation: 1,
        };
        core.handle(EngineMsg::RelayConnected(handle, session.clone()));

        let effects = core.handle(EngineMsg::RelayDisconnected(
            handle,
            session,
            DisconnectReason::Closed,
        ));
        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::EnsureRelay(..))));
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum RelayAckClass {
    Acked,
    Transient(TransientCause),
    WaitingAuth,
    Rejected,
}

fn classify_relay_ack(status: bool, message: &str) -> RelayAckClass {
    if status {
        return RelayAckClass::Acked;
    }
    let Some((prefix, _)) = message.split_once(':') else {
        return RelayAckClass::Rejected;
    };
    match prefix {
        "duplicate" => RelayAckClass::Acked,
        "rate-limited" => RelayAckClass::Transient(TransientCause::RelayRateLimited),
        "error" => RelayAckClass::Transient(TransientCause::RelayError),
        "auth-required" => RelayAckClass::WaitingAuth,
        "invalid" | "pow" | "blocked" | "restricted" | "mute" => RelayAckClass::Rejected,
        _ => RelayAckClass::Rejected,
    }
}

#[cfg(test)]
mod durable_retry_policy_tests {
    use super::*;

    fn key() -> LaneKey {
        LaneKey {
            intent_id: IntentId(42),
            relay: RelayUrl::parse("wss://retry-policy.example").unwrap(),
        }
    }

    #[test]
    fn standardized_ok_prefixes_and_unknown_default_are_exact() {
        assert_eq!(classify_relay_ack(true, "anything"), RelayAckClass::Acked);
        assert_eq!(
            classify_relay_ack(false, "duplicate: already have this event"),
            RelayAckClass::Acked
        );
        assert_eq!(
            classify_relay_ack(false, "rate-limited: slow down"),
            RelayAckClass::Transient(TransientCause::RelayRateLimited)
        );
        assert_eq!(
            classify_relay_ack(false, "error: temporary relay failure"),
            RelayAckClass::Transient(TransientCause::RelayError)
        );
        assert_eq!(
            classify_relay_ack(false, "auth-required: authenticate"),
            RelayAckClass::WaitingAuth
        );
        for prefix in ["invalid", "pow", "blocked", "restricted", "mute"] {
            assert_eq!(
                classify_relay_ack(false, &format!("{prefix}: reason")),
                RelayAckClass::Rejected
            );
        }
        for raw in [
            "unknown: reason",
            "malformed without delimiter",
            "duplicate but only in free-form text",
            "Duplicate: prefix matching is case-sensitive",
            " rate-limited: leading whitespace is not a prefix",
        ] {
            assert_eq!(
                classify_relay_ack(false, raw),
                RelayAckClass::Rejected,
                "free-form relay text must never be heuristically classified: {raw}"
            );
        }
    }

    #[test]
    fn retry_backoff_is_bounded_and_deterministic_from_persisted_identity() {
        let key = key();
        let first = retry_delay_secs(&key, 1);
        assert!((3..8).contains(&first));
        assert_eq!(first, retry_delay_secs(&key, 1));
        for ordinal in 1..=16 {
            let delay = retry_delay_secs(&key, ordinal);
            let exponent = ordinal.saturating_sub(1).min(63) as u32;
            let base = RETRY_INITIAL_SECS
                .checked_shl(exponent)
                .unwrap_or(u64::MAX)
                .min(RETRY_MAX_SECS);
            assert!((base..base + RETRY_JITTER_MAX_SECS).contains(&delay));
        }
        assert!((300..305).contains(&retry_delay_secs(&key, u64::MAX)));
        assert_ne!(
            retry_delay_secs(&key, 1),
            retry_delay_secs(
                &LaneKey {
                    intent_id: IntentId(43),
                    relay: key.relay,
                },
                1
            ),
            "this fixture must prove persisted attempt identity participates in jitter"
        );
    }
}

/// NIP-65 Relay List Metadata — the kind the self-bootstrapping outbox (M5)
/// auto-discovers for any author the current demand references but whose
/// write relays the directory doesn't know yet (see [`EngineCore::
/// sync_discovery`]). Already a member of `nmp_router::DiscoveryKinds`'s
/// default set, so the router routes this atom to the configured indexers
/// with NO router-side changes of its own -- the same `build_candidates`
/// eligibility check that already applies to kind:3/kind:0/kind:10050.
const NIP65_RELAY_LIST_KIND: u16 = 10_002;

pub use admission::RelayAdmissionPolicy;
use attribution::AttributionState;
pub use diagnostics::{
    AuthDiagnosticsPhase, AuthDiagnosticsSnapshot, DiagnosticsSnapshot, FilterCoverageEntry,
    RelayDiagnosticsSnapshot,
};
pub use evidence::{AcquisitionEvidence, AuthPhase, ShortfallFact, SourceEvidence, SourceStatus};
pub use history::{HistoryAdvanceError, HistoryBatch, HistoryQuery, HistorySessionId, WindowLoad};
// `runtime` (C) needs the EXACT same wire subscription-id string
// `attribution.rs` records at send time (`AttributionState::record_send`) so
// that a REQ actually placed on the wire under this string round-trips back
// to the right `SubId` when the relay echoes it in an EOSE — re-derive it or
// drift silently breaks coverage attribution. `pub(crate)` (not a wider
// re-export): this is an internal wire-format detail `core` and `runtime`
// share, never a public contract for callers outside this crate.
pub(crate) use attribution::wire_sub_id_string;

/// Opaque id correlating a `Publish`/`RequestSign` to its `EmitReceipt`/
/// `SignerCompleted`.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct ReceiptId(pub u64);

/// A publish failure that occurs before any receipt identity can exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PublishError {
    /// Every upper-half correlation id has already been issued. No id is
    /// reused, wrapped into the durable lower half, or fabricated.
    ReceiptCorrelationIdExhausted,
    /// The runtime has begun its finite cancellation/drain phase and cannot
    /// accept a new write before closing.
    EngineShuttingDown,
}

impl std::fmt::Display for PublishError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ReceiptCorrelationIdExhausted => {
                write!(f, "receipt correlation id namespace exhausted")
            }
            Self::EngineShuttingDown => write!(f, "engine is shutting down"),
        }
    }
}

impl std::error::Error for PublishError {}

/// Truthful result of trying to attach a receipt observer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReattachOutcome {
    /// The retained receipt and all replay evidence were readable; the sink
    /// was primed and, for live work, registered for subsequent facts.
    Attached,
    /// This store has no retained receipt with the requested id.
    NotFound,
    /// The receipt identity is retained, but its receipt/attempt/route evidence
    /// cannot be decoded. Nothing is published, deleted, or attached.
    RetainedButUnreadable,
}

impl ReattachOutcome {
    pub fn is_attached(self) -> bool {
        self == Self::Attached
    }
}

/// Sink an app-facing `Handle` registers for row deltas on a subscription.
pub trait RowSink: Send {
    fn on_rows(&self, rows: Vec<RowDelta>);
}

/// Reducer-side observer for one coordinated history session. Runtime
/// delivery still travels through [`Effect::EmitHistory`]; this sink keeps
/// the pure headless reducer directly falsifiable like [`RowSink`].
pub trait HistorySink: Send {
    fn on_history(&self, batch: HistoryBatch);
}

/// The canonical row value (#105): the event plus its sorted, deduplicated
/// relay-observation set -- `nmp_store::Provenance::seen`'s keys, projected
/// honestly rather than mirrored into a second parallel provenance store.
/// `sources` only ever grows for a given event id (`Provenance::
/// merge_observation` never removes an entry), so `Row`/`RowDelta` never
/// need a "sources shrank" case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub event: nostr::Event,
    pub sources: BTreeSet<RelayUrl>,
}

/// A row-set delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only). This is the standard reactive-query contract:
/// `Effect::EmitRows`/`RowSink::on_rows` NEVER re-sends the query's full
/// current row set -- only the rows ADDED and REMOVED since that handle's
/// LAST emit (`refresh_handle`'s job). The FIRST emit for a fresh subscribe
/// is "every currently-matching row, as `Added`" (there is nothing to diff
/// against yet); an identity re-root (`set_active_pubkey`) that swaps the
/// whole row set falls out of the SAME diff -- "remove everything old, add
/// everything new" -- with no special-casing. Without this contract, a
/// long-running subscription that keeps matching new events re-delivers its
/// ENTIRE growing row set on every single ingest: O(rows) work per event,
/// O(rows²) total over a session (confirmed live: ~3.35M raw row deliveries
/// for ~2,587 distinct notes in 20s against real relays --
/// `docs/known-gaps.md`'s P0).
#[derive(Debug, Clone)]
pub enum RowDelta {
    /// A row that newly matches the query, carrying the full row (event +
    /// its current relay-provenance set) so the app never has to look
    /// either up separately.
    Added(Row),
    /// The SAME row already matched (#105): its relay-provenance SET grew --
    /// a relay not already in it delivered this exact event id. This is a
    /// `BTreeSet<RelayUrl>` compare, not a timestamp compare: an
    /// already-seen relay redelivering at a strictly later timestamp DOES
    /// advance `nmp_store::Provenance::merge_observation`'s internal
    /// watermark, but the projected SET is unchanged, so it correctly does
    /// NOT fire this variant (the "no spurious update for an identical
    /// observation" bar applies to the set, which is all this surface ever
    /// exposes). The event body itself is unchanged, so only the id and the
    /// row's FULL current source set are carried (matching `Added`'s own
    /// "whole value, not a patch" shape) -- never fired for a no-op
    /// redelivery, and never fired merely because SOME OTHER handle's
    /// lifecycle event forced a `refresh_handle` recompute of this one.
    SourcesGrew {
        id: EventId,
        sources: BTreeSet<RelayUrl>,
    },
    /// A row that no longer matches the query. Carries only the id -- the
    /// app is expected to already hold the event from an earlier `Added`
    /// (raw deltas + coverage only: no second copy of the payload is kept
    /// around just to hand back on removal).
    Removed(EventId),
}

impl RowDelta {
    /// The event id this delta concerns, regardless of variant.
    pub fn id(&self) -> EventId {
        match self {
            RowDelta::Added(row) => row.event.id,
            RowDelta::SourcesGrew { id, .. } => *id,
            RowDelta::Removed(id) => *id,
        }
    }

    /// The event payload, if this is an `Added` delta (`None` for
    /// `SourcesGrew`/`Removed` -- the app is expected to already hold the
    /// event from an earlier `Added`).
    pub fn event(&self) -> Option<&nostr::Event> {
        match self {
            RowDelta::Added(row) => Some(&row.event),
            RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
        }
    }
}

/// Identity of one reducer-owned NIP-42 challenge epoch. The sequence is
/// monotonic for the exact physical session and is never reset by a new
/// transport generation; the handle makes stale-generation completions
/// structurally distinguishable even before the sequence is inspected.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthEpoch {
    pub handle: TransportRelayHandle,
    pub session: RelaySessionKey,
    pub sequence: u64,
}

/// One asynchronous operation inside an [`AuthEpoch`]. Tokens are minted in
/// monotonic order per exact session and are never inferred from challenge
/// text, event ids, the active account, or callback arrival order.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthOpToken {
    pub epoch: AuthEpoch,
    pub sequence: u64,
}

/// App-owned policy's explicit result for one exact AUTH operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthPolicyOutcome {
    Allow,
    Deny { reason: String },
    Unavailable,
    Error { reason: String },
}

/// Signer adapter's explicit result for one exact AUTH operation. A signed
/// event is still untrusted until the reducer verifies the complete frozen
/// template, id, and signature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthSignerOutcome {
    Signed(SignedEvent),
    Unavailable,
    Rejected { reason: String },
    Error { reason: String },
}

/// Result of handing the reducer-validated AUTH event to the exact current
/// physical session. This correlation is intentionally separate from the
/// durable-write [`AttemptCorrelation`] namespace.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthSendOutcome {
    Accepted,
    Unavailable,
}

/// Capability whose removal/replacement invalidates AUTH truth for the
/// frozen expected key. Runtime registries send this after their own exact
/// registration identity check; the reducer never consults mutable current
/// account state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthCapability {
    Policy,
    Signer,
}

/// Opaque identity of one exact registered policy or signer capability.
/// Registries mint this identity; stale removal of an older instance cannot
/// invalidate a replacement because the reducer compares the instance
/// frozen into the current epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct AuthCapabilityInstance(pub u64);

/// The complete reducer-to-runtime AUTH executor vocabulary. Runtime owns
/// execution and cancellation; only the reducer owns epoch truth and phase
/// transitions.
#[derive(Debug)]
pub enum AuthEffect {
    Cancel(AuthEpoch),
    RequestPolicy {
        token: AuthOpToken,
        expected_pubkey: PublicKey,
        challenge: String,
    },
    RequestSignature {
        token: AuthOpToken,
        unsigned: Box<UnsignedEvent>,
    },
    Send {
        token: AuthOpToken,
        epoch: AuthEpoch,
        event: Box<SignedEvent>,
    },
}

/// The read/write/frame vocabulary the reducer consumes (plan §3.4).
pub enum EngineMsg {
    Subscribe(LiveQuery, Box<dyn RowSink>),
    Unsubscribe(HandleId),
    SubscribeHistory(HistoryQuery, Box<dyn HistorySink>),
    /// Declaratively raise this window's row target to at least `usize`,
    /// clamped to the declared `max_rows` (#485). Monotonic and idempotent:
    /// a value at or below the current target is a no-op (or, at the bound, a
    /// single `AtBound` frame beat). Replaces the opaque continuation token.
    RequestRows(HistorySessionId, usize),
    /// Runtime acknowledgement that every newly-required relay worker was
    /// acquired and the staged window advance may become observable.
    CommitHistoryLoad(HistorySessionId),
    /// Runtime refusal/caller cancellation before a staged advance became
    /// observable. Restores the exact prior projection and demand.
    RollbackHistoryLoad(HistorySessionId),
    UnsubscribeHistory(HistorySessionId),
    SetActivePubkey(Option<PublicKey>),
    Publish(WriteIntent, Box<dyn ReceiptSink>),
    RelayConnected(TransportRelayHandle, RelaySessionKey),
    /// Transport completed this exact protected generation's initial socket
    /// observation. Any observed frame was ordered before this edge on the
    /// same worker event stream; public generations never emit it.
    AuthProbeReleased(TransportRelayHandle, RelaySessionKey),
    /// Result of the engine-owned NIP-11 one-shot started for a connected
    /// relay. `Some` retains document revision/freshness/error provenance;
    /// `None` means no document fact was acquired before the decision grace.
    /// Deliberately URL-keyed: NIP-11 is one-shot HTTP evidence about the
    /// relay itself, acquired outside any websocket session (#8: only the
    /// PUBLIC session ever consumes it).
    RelayInformationResolved(RelayUrl, Option<RelayInformationCapabilityEvidence>),
    /// `reason` distinguishes an ordinary transient disconnect (the pool
    /// itself keeps redialing on its own backoff schedule -- the reducer's
    /// job is only to reflect the link status and re-request its worker) from
    /// a `DisconnectReason::PermanentlyFailed` one (401/403 -- the pool has
    /// ALREADY retired the worker for good; see `on_relay_disconnected`'s
    /// doc for why a permanent reason must never re-issue `Effect::
    /// EnsureRelay`, which would otherwise busy-loop against a relay that
    /// keeps saying no) and a `DisconnectReason::Closed` one (an intentional
    /// close must never resurrect the session).
    RelayDisconnected(TransportRelayHandle, RelaySessionKey, DisconnectReason),
    RelayHealth(TransportRelayHandle, RelaySessionKey, RelayHealth),
    /// Runtime could not create a required relay worker. Observational only:
    /// current demand remains the retry owner and diagnostics retain the
    /// exact failure instead of silently presenting a merely connecting
    /// session forever.
    RelayOpenFailed(RelaySessionKey, String),
    RelayFrame(TransportRelayHandle, RelaySessionKey, RelayFrame),
    RelayFrames(Vec<(TransportRelayHandle, RelaySessionKey, RelayFrame)>),
    SignerCompleted(ReceiptId, u64, Result<SignedEvent, SignerError>),
    /// The runtime has no signer attached for this accepted author. This is
    /// non-terminal: the canonical pending row and durable obligation stay
    /// alive until a matching signer is attached or the app cancels.
    SignerUnavailable(ReceiptId, u64),
    /// A capability for this author was attached. Re-arm every matching
    /// accepted unsigned intent through the ordinary RequestSign effect.
    SignerAttached(PublicKey),
    AuthPolicyCompleted(
        AuthOpToken,
        Option<AuthCapabilityInstance>,
        AuthPolicyOutcome,
    ),
    AuthSignerCompleted(
        AuthOpToken,
        Option<AuthCapabilityInstance>,
        AuthSignerOutcome,
    ),
    /// Runtime atomically snapped this exact capability instance before
    /// starting the asynchronous operation named by `token`. Binding is a
    /// reducer input, not inferred from whichever instance later completes.
    AuthCapabilityBound {
        token: AuthOpToken,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    },
    AuthSendCompleted(AuthOpToken, AuthSendOutcome),
    AuthCapabilityInvalidated(PublicKey, AuthCapability, AuthCapabilityInstance),
    /// Explicit pre-signature cancellation. Once promotion has committed,
    /// cancellation cannot retract a valid signed cache row.
    CancelWrite(ReceiptId),
    /// The one, ever, typed result of a durable `EVENT` handoff (issue
    /// #93), translated from `PoolEvent::EventHandoff`. See
    /// `EngineCore::on_event_handoff`'s doc for what this does and does
    /// NOT do in this unit.
    EventHandoff(AttemptCorrelation, HandoffResult),
    Tick(Timestamp),
}

/// The row/wire/receipt vocabulary the reducer emits (plan §3.4). `EmitRows`
/// carries the query's [`AcquisitionEvidence`] alongside its rows
/// (`docs/design/scoped-evidence-49-12-plan.md`): per-source acquisition
/// facts over the query's FULL subtree (interior `Derived` atoms included,
/// #12), never a single collapsed query-global verdict — an app reads
/// which source has proven what, it is never handed a settled/complete
/// judgment.
#[derive(Debug)]
pub enum Effect {
    /// -> `Pool::send` per (relay, current handle).
    Wire(WireDelta),
    /// Prospective relay-session workers for a staged history advance. The
    /// runtime may preflight these workers, but dispatch never sends protocol
    /// work from this effect. The live router/attribution state changes only
    /// after the synchronous caller has accepted the successful reply. Keyed
    /// by full [`RelaySessionKey`] (#8): the staged shadow plan's demand
    /// atoms carry their access context, and preflighting the URL's PUBLIC
    /// session for a protected atom would acquire the wrong physical worker.
    PreflightHistoryRelays(BTreeSet<RelaySessionKey>),
    /// Reconnect: resend the current wire subs on the NEW generation of
    /// exactly this session.
    Replay(RelaySessionKey, Vec<WireReq>),
    /// Acquire/revalidate NIP-11 without blocking the reducer thread.
    FetchRelayInformation(RelayUrl),
    /// Open the exact protected transport generation's ordinary outbound gate
    /// after its ordered initial-read edge is applied, or required AUTH
    /// completes.
    ReleaseInitialRead(TransportRelayHandle),
    /// Place a capability-probing `NEG-OPEN` on the wire (`negentropy::
    /// Prober::begin_probe`'s output, carried in full since the runtime has
    /// no negentropy-protocol knowledge of its own): the sub-id, the
    /// throwaway probe filter, and the hex initial message.
    StartProbe(RelayUrl, SubId, ConcreteFilter, String),
    /// Place a REAL negentropy-first `NEG-OPEN` for `filter` against a
    /// PROVEN-supported relay (ledger #8's compile-fence: the first field
    /// can only ever be a `ProbedRelay`), under `sub_id`, with the hex
    /// initial message this reducer already built from its own store.
    NegOpen(ProbedRelay, SubId, ConcreteFilter, String),
    /// Continue an open reconciliation: place this hex payload as the next
    /// outbound `NEG-MSG` for `sub_id` on `relay`.
    NegMsg(RelayUrl, SubId, String),
    /// Release `sub_id` on `relay` (`NEG-CLOSE`) -- reconciliation finished,
    /// was abandoned (liveness deadline / `NEG-ERR`), or is being converted
    /// back to a plain REQ.
    NegClose(RelayUrl, SubId),
    /// One per attributed atom per EOSE/NEG-DONE (ruling §7): the narrow
    /// atom's `CoverageKey`, the relay that proved it, and the proven
    /// interval.
    RecordCoverage(
        nmp_store::CoverageKey,
        RelayUrl,
        nmp_store::CoverageInterval,
    ),
    EmitRows(HandleId, Vec<RowDelta>, AcquisitionEvidence),
    EmitHistory(HistorySessionId, HistoryBatch),
    HistoryLoadResult(HistorySessionId, Result<(), HistoryAdvanceError>),
    /// The engine-global diagnostics projection (M5 plan §1.2 step 3),
    /// pushed at the end of every `recompile()` and after every EOSE
    /// (coverage watermarks can advance with no recompile at all). Read-only
    /// and off the data path -- never influences routing/delivery.
    /// `runtime::Handle::observe_diagnostics` forwards this to every
    /// registered observer, latest-wins if a consumer is slow (never
    /// buffered/replayed).
    EmitDiagnostics(DiagnosticsSnapshot),
    EmitReceipt(ReceiptId, WriteStatus),
    /// The publish could not even allocate a non-durable correlation id,
    /// so no `EmitReceipt` can truthfully accompany this failure.
    PublishFailed(PublishError),
    RequestSign(ReceiptId, u64, UnsignedEvent),
    /// Execute one reducer-owned NIP-42 operation. This envelope has its own
    /// epoch/token and never reuses durable-write signing or handoff
    /// correlations.
    RelayAuth(AuthEffect),
    /// A remote signer became available again before its previous retryable
    /// completion reached the engine. The runtime checks the currently
    /// registered capability's live availability before sending the ordinary
    /// `SignerAttached` event, closing that cross-thread ordering race.
    RearmSignerIfAvailable(PublicKey),
    RequestDecrypt(EventId, PublicKey, String),
    /// Outbox: publish `event` to `relay` (plan §3.4's "`Effect::Wire`
    /// publish REQ/EVENT per relay", re-cut as its OWN effect rather than a
    /// `nmp_router::WireOp` variant — `WireOp`/`WireDelta` are read-
    /// subscription vocabulary owned by `nmp-router`, out of this builder's
    /// scope to extend; this is engine-owned wire vocabulary for the write
    /// plane). C (runtime) translates this to `Pool::send_durable` of an
    /// `["EVENT", …]` frame on `relay`'s current generation, correlated by
    /// `AttemptCorrelation` (issue #93) — the durable handoff is generation-
    /// scoped and reports back exactly one typed `HandoffResult`, never
    /// silently carried into a later connection. Since the AUTH-reducer wave
    /// (#8 U2) the write plane rides the lane's identity-scoped
    /// authenticated session — `RelaySessionKey::new(relay,
    /// AccessContext::Nip42(signing pubkey))` — never the relay's Public
    /// read session: the reducer that can actually authenticate that
    /// session now exists, and an OK is only ever trusted from the exact
    /// session the write was published on.
    PublishEvent(RelaySessionKey, SignedEvent, AttemptCorrelation),
    /// Ensure a write-only relay session is dialing without creating an
    /// attempt. An ordinal is allocated only after `RelayConnected` proves
    /// the session online, so offline time consumes zero attempts.
    EnsureRelay(RelaySessionKey),
}

/// Per-handle bookkeeping `EngineCore` must retain across `handle()` calls:
/// the `QueryHandle` itself (dropping it would withdraw the subscription —
/// see `nmp_resolver::QueryHandle`'s `Drop` impl), the app-facing sink, and
/// the last-emitted row/evidence state (so `EmitRows` fires only when
/// something actually changed, not on every unrelated recompile).
/// `AcquisitionEvidence` derives `PartialEq` precisely so this
/// change-detection compare stays a plain value comparison, as the former
/// query-evidence aggregate's did. `last_rows` maps each currently-matching
/// id to the SOURCE SET last emitted for it (#105) -- not just the id --
/// so `refresh_handle` can detect provenance growth on an already-matching
/// row the SAME way it already detects `Added`/`Removed`: a plain value
/// compare against this remembered state, never a second bespoke mechanism.
struct HandleState {
    _handle: QueryHandle,
    sink: Box<dyn RowSink>,
    last_rows: BTreeMap<EventId, RememberedRow>,
    last_evidence: Option<AcquisitionEvidence>,
    /// False after any failed full refresh. Direct deltas cannot repair a
    /// possibly missed historical snapshot, so the next affected batch must
    /// retry the full oracle before incremental application resumes.
    projection_complete: bool,
}

struct HistoryState {
    query: HistoryQuery,
    /// Resolver handles the session currently holds open: the one live-top
    /// demand (`live_handle_id`) plus at most the *current* advance's
    /// tie-second/older acquisition handles. Older advances' historical
    /// acquisitions are closed at the next commit (#486) so a deep scroll of
    /// `K` advances never accumulates `O(K)` live relay subscriptions.
    handles: Vec<QueryHandle>,
    handle_ids: BTreeSet<HandleId>,
    /// The initial, permanent live-top demand opened at
    /// [`Self::on_subscribe_history`]. It is never a historical acquisition
    /// and is retired only when the whole session is dropped.
    live_handle_id: HandleId,
    /// Every engine-owned acquisition handle the session currently holds open,
    /// mapped to `Some(second)` for a tie-second REQ (`since==until==second`)
    /// or `None` for an older-range REQ. The live-top handle is never in this
    /// map. This is what the #486 supersede-close consults: an older handle is
    /// always safe to retire once superseded (its range is re-requestable, so
    /// no permanent gap), while a tie handle is kept open until the window
    /// boundary descends strictly below its second — only then is that dense
    /// second fully materialized as an interior region and its REQ redundant,
    /// so retiring it can never drop an un-projected same-second row.
    acquisitions: BTreeMap<HandleId, Option<u64>>,
    sink: Box<dyn HistorySink>,
    target_rows: usize,
    acquired_tie_seconds: BTreeSet<u64>,
    /// The bounded canonical payload set. History delivery is latest-wins,
    /// so every emitted frame must be able to stand alone after intermediate
    /// deltas are overwritten.
    last_rows: BTreeMap<EventId, Row>,
    /// Same membership as `last_rows`, ordered canonically newest-first.
    /// This makes top/bottom rebalance O(log max_rows), never an O(total)
    /// sort after every committed row mutation.
    order: BTreeSet<(Reverse<u64>, EventId)>,
    last_evidence: Option<AcquisitionEvidence>,
    projection_complete: bool,
    load: WindowLoad,
    pending_load: Option<PendingHistoryLoad>,
}

struct PendingHistoryLoad {
    prior_target_rows: usize,
    prior_load: WindowLoad,
    prior_evidence: Option<AcquisitionEvidence>,
    prior_projection_complete: bool,
    acquired_tie_second: Option<u64>,
    opened_handle_ids: Vec<HandleId>,
    added_row_ids: Vec<EventId>,
    staged_batches: Vec<HistoryBatch>,
}

/// The minimal retained projection state needed to apply a committed writer
/// delta without re-materializing the handle's entire history. Event bodies
/// still live only in the store/app delta; the engine remembers selection and
/// provenance keys, not a second payload cache.
#[derive(Debug, Clone, PartialEq, Eq)]
struct RememberedRow {
    created_at: u64,
    sources: BTreeSet<RelayUrl>,
}

/// Per-receipt bookkeeping the reducer retains from `Publish` through to the
/// last per-relay ack (or `Ephemeral`'s generation-scoped handoff effects).
/// Ephemeral still owns a receipt-only record and status stream; what it
/// lacks is a durable delivery obligation and canonical pending row.
struct PendingWrite {
    durability: Durability,
    routing: WriteRouting,
    /// False only when a persisted routing snapshot cannot be decoded.
    /// Recovery keeps owning the obligation but fails closed on wire output.
    routing_valid: bool,
    /// Zero or more observers. Recovery owns the obligation even before an
    /// app reattaches, and multiple observers may follow the same receipt.
    sinks: Vec<Rc<dyn ReceiptSink>>,
    /// Store-allocated durable intent id. `None` only for Ephemeral's
    /// receipt-only path, which never owns a pending row.
    intent_id: Option<IntentId>,
    /// Signer identity selected and frozen at acceptance. Later active-
    /// account changes cannot redirect this obligation.
    signing_pubkey: PublicKey,
    /// Exact frozen body accepted by the store (sentinel signature). Kept
    /// so signer responses can be validated byte-for-byte before promotion
    /// and so compensation can invalidate the ordinary resolver graph.
    frozen: SignedEvent,
    /// True when `accept_write` found an already-signed duplicate and
    /// journaled this co-owner as Signed immediately.
    already_signed: bool,
    /// Exactly one signer operation may be outstanding for an intent.
    /// Attach/activate notifications are idempotent while this is true.
    sign_request_in_flight: bool,
    sign_generation: u64,
    /// Set once the signer resolves; used to clean up `event_to_receipt`.
    event_id: Option<EventId>,
    /// Relays sent-to but not yet terminal (acked/rejected/given-up).
    /// Durable and AtMostOnce both populate this (both track real per-relay
    /// state); AtMostOnce's distinguishing property is that NOTHING in this
    /// reducer ever re-sends on a `RelayDisconnected` for either class — a
    /// dropped pending relay always resolves to `GaveUp`, never a retry
    /// `PublishEvent` (no blind retry, ledger's `AtMostOnce` amendment).
    pending_relays: BTreeSet<RelayUrl>,
    /// Routed lanes for which `start_lane_attempt` failed. They remain
    /// explicitly owned and nonterminal, but never enter `pending_relays`
    /// because no Started fact exists and no wire EVENT was emitted.
    unstarted_relays: BTreeSet<RelayUrl>,
    /// Resolved URLs whose route revision did not persist. Owned only for
    /// this process lifetime; crash recovery may re-resolve policy but cannot
    /// claim these exact URLs durably.
    route_blocked_relays: BTreeSet<RelayUrl>,
    /// The persisted started ordinal currently awaiting a terminal outcome
    /// for each relay.
    attempt_ordinals: BTreeMap<RelayUrl, u64>,
    /// Every relay this reducer has ever learned owns a persisted outbox
    /// lane for this intent (epic #507 finding E5). Populated exactly where
    /// the core learns an intent's lanes — `bootstrap_outbox_lanes`'s two
    /// call sites (`recover_on_boot`, `on_signed`) — and never elsewhere:
    /// this is the per-receipt half of `EngineCore::receipts_by_lane_relay`,
    /// kept so a permanent removal from `pending` can walk exactly this set
    /// to clean the reverse index rather than scanning it.
    lane_relays: BTreeSet<RelayUrl>,
}

/// A live, EngineCore-owned negentropy reconciliation in progress for
/// `sub_id` (plan §6 E). `filter` is already window-erased (since/until/
/// limit cleared) -- ruling §2: "NEG runs unfloored/unlimited"; recording an
/// attribution snapshot straight off this field is therefore always the
/// correct floor:None/until:None/limited:false snapshot the ruling
/// requires, with no separate bookkeeping to keep in sync.
struct NegSession {
    relay: RelayUrl,
    filter: ConcreteFilter,
    absorbed: BTreeSet<CoverageKey>,
    started_at: Timestamp,
    reconciler: Reconciler,
}

#[derive(Debug)]
struct AuthSessionState {
    epoch: AuthEpoch,
    challenge: String,
    last_created_at: Option<Timestamp>,
    policy_instance: Option<AuthCapabilityInstance>,
    signer_instance: Option<AuthCapabilityInstance>,
    phase: AuthSessionPhase,
}

#[derive(Debug)]
enum AuthSessionPhase {
    AwaitingPolicy {
        token: AuthOpToken,
    },
    AwaitingSignature {
        token: AuthOpToken,
        unsigned: UnsignedEvent,
    },
    AwaitingSend {
        token: AuthOpToken,
        event_id: EventId,
        early_ok: Option<bool>,
    },
    AwaitingOk {
        event_id: EventId,
    },
    Ready {
        event_id: EventId,
    },
    Denied,
    Error,
}

/// The PURE synchronous reducer (§2 position 1). No I/O, no threads.
pub struct EngineCore<S: EventStore> {
    resolver: ResolverEngine<S>,
    router: Router,
    directory: Box<dyn RelayDirectory>,
    cap: usize,
    handles: HashMap<HandleId, HandleState>,
    histories: HashMap<HistorySessionId, HistoryState>,
    history_by_handle: HashMap<HandleId, HistorySessionId>,
    next_history_id: u64,
    attribution: AttributionState,
    /// EngineCore's memory of the exact connection generation and SESSION
    /// that currently occupy each pool slot. Disconnects are asynchronous;
    /// the generation prevents a delayed old disconnect from erasing a slot
    /// that has already reopened, and the session key prevents a frame
    /// reported for one access context from ever being read as another's
    /// (#8: both halves of the (handle, session) pair must match exactly).
    slot_to_relay: HashMap<u32, (TransportRelayHandle, RelaySessionKey)>,
    /// Sessions CURRENTLY connected — feeds `AcquisitionEvidence.sources[_]
    /// .status` (`Requesting` iff a member here covers the atom;
    /// `Disconnected` iff it was a member of `ever_connected_relays` but
    /// isn't a member here; `Connecting` otherwise). Additive bookkeeping:
    /// `slot_to_relay`'s own semantics (populated on connect, never cleared on
    /// disconnect) are untouched by this.
    connected_relays: BTreeSet<RelaySessionKey>,
    /// Every session that has connected at least once, ever — distinguishes
    /// `Disconnected` (was connected, dropped) from `Connecting` (never yet
    /// connected) for the same evidence computation.
    ever_connected_relays: BTreeSet<RelaySessionKey>,
    /// The exact connection generation that has completed NIP-42 AUTH for
    /// each PROTECTED session (#8). Public sessions never enter this map. A
    /// fresh generation is never pre-authorized (`on_relay_connected` removes
    /// the entry), and readiness dies with the connection
    /// (`on_relay_disconnected` removes it too) — so "ready" always means
    /// "THIS socket, after THIS socket's AUTH handshake", never an earlier
    /// generation's leftover.
    auth_ready_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Newly connected author sessions whose first inbound frame is still
    /// being observed for a proactive AUTH challenge. Unlike sticky
    /// `auth_required_sessions`, this exact-generation gate is released by a
    /// transport's ordered first-read completion when an ordinary relay has
    /// no already-available challenge.
    auth_probe_sessions: HashMap<RelaySessionKey, TransportRelayHandle>,
    /// Exact live sessions for which the relay has actually required AUTH:
    /// an AUTH challenge, auth-required write response, or restricted close.
    /// Merely using a frozen NIP-42 access identity does not populate this
    /// set; ordinary relays are released only after the transport's ordered
    /// first socket read-drain completes without an available challenge.
    auth_required_sessions: BTreeSet<RelaySessionKey>,
    /// Current reducer-owned AUTH epoch for each exact protected session.
    /// Entries are removed on disconnect/reconnect teardown; the separate
    /// monotonic counters below deliberately survive that removal so stale
    /// callbacks can never alias a future generation.
    auth_sessions: HashMap<RelaySessionKey, AuthSessionState>,
    next_auth_epoch: Option<u64>,
    next_auth_operation: Option<u64>,
    /// Persisted ordinary-write rows of reserved kind:22242 discovered at
    /// boot. They remain durably inspectable but never regain reducer
    /// ownership, attempt correlations, or a reattachable live sink.
    quarantined_auth_receipts: HashMap<ReceiptId, String>,
    clock: Timestamp,
    active_pubkey: Option<PublicKey>,
    /// Correlation ids for failures that were never accepted use the upper
    /// half of the namespace. Store-issued durable ids occupy the lower half
    /// and advance independently, so reattachment can never alias one.
    next_unaccepted_receipt: Option<u64>,
    /// Write outbox (§3.4 / VISION §7 ledger #6/#9). `pending` is keyed by
    /// `ReceiptId` from `Publish` through to the last terminal per-relay
    /// status; `event_to_receipt` lets an inbound `OK` frame (keyed by
    /// `EventId` on the wire) find its receipt.
    pending: HashMap<ReceiptId, PendingWrite>,
    event_to_receipts: HashMap<EventId, BTreeSet<ReceiptId>>,
    /// O(1) reverse index of `pending`'s own `intent_id` field (epic #507
    /// finding E5): `receipt_for_intent` used to be a full linear scan of
    /// `pending`, run once per due deadline in
    /// `consume_due_outbox_deadlines`. Maintained at every real
    /// `pending.insert`/`pending.remove` (never at `fail_and_compensate`'s
    /// transient remove-then-reinsert, which never changes which intent a
    /// receipt owns). This mirrors `pending` exactly and needs no separate
    /// invalidation story: it is rebuilt from scratch, in step with
    /// `pending`, every `recover_on_boot`.
    intent_receipts: HashMap<IntentId, ReceiptId>,
    /// Relay -> receipts with a lane on that relay (epic #507 finding E5).
    /// A narrowing INDEX only, never a second source of truth: the store's
    /// `OUTBOX_LANES` table stays authoritative (its keys are intent-first,
    /// and `close_terminal_intent` deliberately never deletes a closed
    /// intent's own terminal lane rows -- both `MemoryStore` and `RedbStore`
    /// only drop `OUTBOX_INTENTS`/the deadline indexes there, per that
    /// door's own doc comment: "Receipts and all route/attempt/detail
    /// evidence are retained" -- so a durable relay-scoped secondary table
    /// would still index retained garbage and would need transactional
    /// maintenance across every lane-writing door).
    /// This index instead rides the reducer's own `pending`/`recover_on_boot`
    /// lifecycle: rebuilt deterministically at boot, so there is no cache-
    /// invalidation question distinct from the one `pending` itself already
    /// answers. `wake_relay_lanes` uses this to avoid re-reading every
    /// outstanding write's lanes on every relay connect/disconnect/auth
    /// event -- it only narrows WHICH intents to re-read via
    /// `recover_outbox_lanes`, the store read itself remains the truth.
    /// Kept in lockstep with each `PendingWrite::lane_relays` (its per-
    /// receipt half): populated at the same two `bootstrap_outbox_lanes`
    /// call sites, cleaned by walking `lane_relays` on a real removal.
    receipts_by_lane_relay: HashMap<RelayUrl, BTreeSet<ReceiptId>>,
    /// Safety valve for `receipts_by_lane_relay` (epic #507 finding E5): set
    /// to true the moment ANY path could have created/learned lanes but the
    /// index could not record them (a `bootstrap_outbox_lanes` or
    /// `recover_route_revisions` error during `recover_on_boot`/`on_signed`).
    /// `recover_on_boot` resets it to false at the start of its one-shot,
    /// deterministic rebuild -- the same moment `pending` itself is rebuilt
    /// from scratch -- and a later failure during that same rebuild (or any
    /// post-boot lane-learning call) sets it back to true for the rest of
    /// this process's life; nothing un-degrades it mid-process, on purpose.
    /// While true, `wake_relay_lanes` falls back to the full
    /// `recover_all_lanes` scan unchanged: a missed wakeup permanently wedges
    /// a durable write lane (the worst bug class here -- see the idle-
    /// barrier missed-wakeup fix, d755f39, and #507's own missed-wakeup
    /// finding), so an unprovable index is always treated as untrustworthy
    /// rather than guessed at.
    lane_relay_index_degraded: bool,
    /// The negentropy capability-probe cache (plan §6 E).
    prober: Prober,
    /// Latest provenance-bearing NIP-11 advertisement for relays in the
    /// current read plan. Recompile pruning and completion-time plan checks
    /// prevent historical relay churn from becoming a shadow cache. This is
    /// kept separate from `prober`: advertisement is evidence, never proof.
    nip11_information: HashMap<RelayUrl, RelayInformationCapabilityEvidence>,
    /// Live reconciliation sessions, keyed by the SAME `SubId` a plain REQ
    /// for this shape would have used (REQ and negentropy share one
    /// subscription-id namespace on the wire, NIP-77) -- never more than one
    /// entry per sub-id at a time.
    neg_sessions: HashMap<SubId, NegSession>,
    /// One-shot `ids`-filter REQs opened to backfill exactly what a
    /// completed reconciliation proved we are missing (`finish_neg_session`)
    /// -- tracked so this reducer closes them itself once their EOSE
    /// arrives, rather than leaking a subscription the router's own
    /// demand-diffing does not know about.
    pending_backfills: BTreeSet<SubId>,
    /// Backfill `SubId` -> the reconciled negentropy session's own `SubId`,
    /// whose coverage credit is deferred until THIS backfill's EOSE proves
    /// the missing events actually landed (ledger #7 -- see
    /// `finish_neg_session`'s doc comment).
    pending_neg_credit: HashMap<SubId, SubId>,
    /// The self-bootstrapping outbox (M5): an internal, engine-owned
    /// resolver subscription discovering kind:10002 for exactly the authors
    /// current demand references but whose write relays are still unknown
    /// (see [`Self::sync_discovery`]). `None` when no author currently needs
    /// discovering. The app never sees this handle or this atom -- it rides
    /// the SAME demand/atom/router machinery every other subscription does,
    /// never a parallel subscription system.
    discovery_handle: Option<QueryHandle>,
    /// The exact author set `discovery_handle` (if any) is currently open
    /// for -- compared against the freshly-computed "needed" set on every
    /// `sync_discovery` call so the subscription is only replaced when the
    /// set actually changes, not on every recompile.
    discovery_authors: BTreeSet<PubkeyHex>,
    /// The diagnostic surface's own counter (M5 plan §1.2 step 1) — events
    /// actually RECEIVED, per SESSION per kind. Bumped in the
    /// `RelayMessage::Event` arms of `on_relay_frame`/`on_relay_frames`;
    /// read (never mutated) by `diagnostics_snapshot`. This is the one datum
    /// `nmp-router`'s `Diagnostics` cannot see on its own — it never
    /// observes inbound frames, only what was compiled/sent. Wire-observed
    /// counts retain the exact physical session (#8) instead of copying one
    /// URL aggregate into every access-context row.
    events_by_session_kind: HashMap<RelaySessionKey, BTreeMap<u16, u64>>,
    /// Next transport-native [`AttemptCorrelation`] to mint (issue #93).
    /// Purely volatile/in-process — never persisted, never restart-durable
    /// (the plan's own words: "no persistence migration" for this unit).
    /// Checked, typed exhaustion, same discipline as
    /// `next_unaccepted_receipt` above.
    next_attempt_correlation: Option<u64>,
    /// `AttemptCorrelation` -> which receipt/relay it was minted for. Engine-
    /// owned bookkeeping only; transport never needs to understand this
    /// mapping, only echo the correlation back unchanged. An entry is
    /// removed the instant its one-and-only `HandoffResult` arrives — see
    /// `Self::on_event_handoff`.
    attempt_correlations: HashMap<AttemptCorrelation, AttemptCorrelationTarget>,
    /// The provenance-aware relay admission policy for DISCOVERED relays
    /// (issue #121). Applied in [`Self::ingest_relay_list_winner`], the one
    /// choke point where a kind:10002 winner's relays become routable lanes.
    /// Defaults to the secure policy (reject every discovered private/
    /// loopback/onion host); production threads the operator's opt-in local
    /// allowlist via [`Self::with_relay_admission`].
    admission: RelayAdmissionPolicy,
    /// Monotonic count of DISCOVERED relay-lane rejections by `admission`
    /// before they could become router candidates (issues #121/#11).
    /// Kind:10002 is counted PER LANE: write and read sets are filtered
    /// separately, so one hostile event naming `N` rejected hosts bumps this
    /// by up to `2N`. Selector-projected facts count once when a rejected
    /// `(selection, evidence)` first enters current demand, not again on an
    /// unchanged recompile. Surfaced in
    /// [`DiagnosticsSnapshot::discovered_private_relays_rejected`]; the
    /// separate worker-exhaustion cap count lives in the pool
    /// (`nmp_transport::Pool::admission_rejections`) and is folded in by the
    /// runtime.
    discovered_private_relays_rejected: u64,
    /// Rejected selector-projected routing facts present at the previous
    /// recompile. Diffing this set prevents an unchanged demand from
    /// inflating the monotonic rejection counter on every reducer pass.
    rejected_projected_evidence: BTreeSet<(DescriptorHash, RoutingEvidence)>,
    /// Read-only degrade flag (issue #122): set once the first time an
    /// ingest/read [`EventStore`] door returns [`PersistenceError`] (disk
    /// full, I/O error). The reducer NEVER panics on such a failure — it
    /// records the error message here, skips the affected reactive step
    /// (leaving already-delivered state untouched rather than fabricating a
    /// phantom retraction), and surfaces it on the read-only diagnostics
    /// snapshot. A minimal, honest "the local cache went read-only" signal;
    /// a richer failure-mode framework (recovery, reopen, per-door policy)
    /// is deliberately out of scope — see the issue's priority note.
    ///
    /// This flag is OBSERVATIONAL, not a gate: no code path reads it to
    /// refuse work. "Read-only" is descriptive — a later message simply
    /// re-attempts the same door and degrades again on a repeat failure
    /// (harmless: every widened door is atomic, so a failed attempt commits
    /// nothing). Enforcing degrade (short-circuiting further writes) would be
    /// the richer policy explicitly deferred here.
    store_degraded: Option<String>,
    /// Runtime relay-worker open failures keyed by their exact current owner.
    /// Entries are pruned whenever demand/write ownership changes and cleared
    /// by a successful connection for that session.
    relay_open_failures: BTreeMap<RelaySessionKey, String>,
    /// Transport health/verifier degradation from a live worker. Kept
    /// separate from open failures so clearing one recovered session cannot
    /// erase an independent transport-health fact.
    transport_degraded: Option<String>,
    /// A failed durable-lane deadline transition is removed from the armed
    /// deadline set until another real engine message retries the reducer.
    /// This prevents a persistent I/O error from becoming recv_timeout(0)
    /// busy-spin while retaining the due row durably for recovery.
    retry_scheduler_blocked: bool,
    /// Test-only work counters for the affected-handle invalidation
    /// falsifier. Production pays no field or increment cost.
    #[cfg(test)]
    projection_store_queries: Cell<u64>,
    #[cfg(test)]
    router_compiles: Cell<u64>,
    #[cfg(test)]
    history_store_queries: Cell<u64>,
    #[cfg(test)]
    history_rows_examined: Cell<u64>,
    #[cfg(test)]
    history_affected_row_queries: Cell<u64>,
}

/// What one `AttemptCorrelation` (issue #93) resolves back to in this
/// reducer's own bookkeeping.
struct AttemptCorrelationTarget {
    receipt: ReceiptId,
    /// The write session this attempt rides: the lane's identity-scoped
    /// authenticated session (`Nip42(signing pubkey)`, #8 U2) — an OK is
    /// only ever trusted from the exact session the write published on.
    session: RelaySessionKey,
    /// Durable/AtMostOnce correlations identify the exact persisted lane
    /// ordinal. Ephemeral correlations have no outbox row.
    lane: Option<(IntentId, u64)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AttemptCorrelationExhausted;

impl<S: EventStore> EngineCore<S> {
    pub fn new(store: S, directory: Box<dyn RelayDirectory>, cap: usize) -> Self {
        Self {
            resolver: ResolverEngine::new(store),
            router: Router::new(
                DiscoveryKinds::default(),
                RuleRegistry::default_widen_only(),
            ),
            directory,
            cap,
            handles: HashMap::new(),
            histories: HashMap::new(),
            history_by_handle: HashMap::new(),
            next_history_id: 1,
            attribution: AttributionState::new(),
            slot_to_relay: HashMap::new(),
            connected_relays: BTreeSet::new(),
            ever_connected_relays: BTreeSet::new(),
            auth_ready_sessions: HashMap::new(),
            auth_probe_sessions: HashMap::new(),
            auth_required_sessions: BTreeSet::new(),
            auth_sessions: HashMap::new(),
            next_auth_epoch: Some(1),
            next_auth_operation: Some(1),
            quarantined_auth_receipts: HashMap::new(),
            clock: Timestamp::from(0u64),
            active_pubkey: None,
            next_unaccepted_receipt: Some(u64::MAX),
            pending: HashMap::new(),
            event_to_receipts: HashMap::new(),
            intent_receipts: HashMap::new(),
            receipts_by_lane_relay: HashMap::new(),
            lane_relay_index_degraded: false,
            prober: Prober::new(),
            nip11_information: HashMap::new(),
            neg_sessions: HashMap::new(),
            pending_backfills: BTreeSet::new(),
            pending_neg_credit: HashMap::new(),
            discovery_handle: None,
            discovery_authors: BTreeSet::new(),
            events_by_session_kind: HashMap::new(),
            next_attempt_correlation: Some(0),
            attempt_correlations: HashMap::new(),
            admission: RelayAdmissionPolicy::default(),
            discovered_private_relays_rejected: 0,
            rejected_projected_evidence: BTreeSet::new(),
            store_degraded: None,
            relay_open_failures: BTreeMap::new(),
            transport_degraded: None,
            retry_scheduler_blocked: false,
            #[cfg(test)]
            projection_store_queries: Cell::new(0),
            #[cfg(test)]
            router_compiles: Cell::new(0),
            #[cfg(test)]
            history_store_queries: Cell::new(0),
            #[cfg(test)]
            history_rows_examined: Cell::new(0),
            #[cfg(test)]
            history_affected_row_queries: Cell::new(0),
        }
    }

    /// Thread the operator's discovered-relay admission policy through
    /// construction (issue #121). Chained onto [`Self::new`] by the runtime
    /// (`engine_loop`); left at the secure default (reject every discovered
    /// private/loopback/onion host) everywhere else, so every test and every
    /// caller that does not opt local hosts in is fail-closed by default.
    #[must_use]
    pub fn with_relay_admission(mut self, admission: RelayAdmissionPolicy) -> Self {
        self.admission = admission;
        self
    }

    /// Record an ingest/read persistence failure (issue #122) without
    /// panicking: latch the first error message (read-only degrade) and push
    /// a fresh diagnostics snapshot so an observer sees the degraded state
    /// immediately. Idempotent — a later failure keeps the first message.
    fn degrade_store(&mut self, err: PersistenceError, effects: &mut Vec<Effect>) {
        if self.store_degraded.is_none() {
            self.store_degraded = Some(err.to_string());
        }
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
    }

    /// Mint the next [`AttemptCorrelation`] (issue #93). Checked, typed
    /// exhaustion -- same discipline as [`Self::alloc_receipt_id`]'s
    /// `next_unaccepted_receipt` counter.
    fn alloc_attempt_correlation(
        &mut self,
    ) -> Result<AttemptCorrelation, AttemptCorrelationExhausted> {
        let id = self
            .next_attempt_correlation
            .ok_or(AttemptCorrelationExhausted)?;
        self.next_attempt_correlation = id.checked_add(1);
        Ok(AttemptCorrelation(id))
    }

    /// O(1) via `intent_receipts` (epic #507 finding E5) -- this door used
    /// to be a full `self.pending` linear scan, run once per due deadline in
    /// `consume_due_outbox_deadlines`.
    fn receipt_for_intent(&self, intent_id: IntentId) -> Option<ReceiptId> {
        self.intent_receipts.get(&intent_id).copied()
    }

    /// Remove a permanently-discarded pending write's entries from the
    /// `intent_receipts` and `receipts_by_lane_relay` indexes (epic #507
    /// finding E5). Call this at every REAL removal from `self.pending` --
    /// never at `fail_and_compensate`'s transient remove-then-reinsert
    /// (`CompensateOutcome::NotFound`/`Err`), which must leave both indexes
    /// untouched because the obligation and its lanes are still live.
    fn forget_pending_indexes(&mut self, id: ReceiptId, pending: &PendingWrite) {
        if let Some(intent_id) = pending.intent_id {
            self.intent_receipts.remove(&intent_id);
        }
        for relay in &pending.lane_relays {
            if let Some(receipts) = self.receipts_by_lane_relay.get_mut(relay) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.receipts_by_lane_relay.remove(relay);
                }
            }
        }
    }

    fn emit_write_status(&self, id: ReceiptId, status: WriteStatus, effects: &mut Vec<Effect>) {
        if let Some(pending) = self.pending.get(&id) {
            Self::notify(pending, status.clone());
        }
        effects.push(Effect::EmitReceipt(id, status));
    }

    fn remove_active_lane(&mut self, id: ReceiptId, relay: &RelayUrl) {
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.pending_relays.remove(relay);
            pending.attempt_ordinals.remove(relay);
        }
    }

    fn close_if_all_lanes_terminal(&mut self, id: ReceiptId) {
        let Some((intent_id, event_id)) = self
            .pending
            .get(&id)
            .filter(|pending| pending.route_blocked_relays.is_empty())
            .and_then(|pending| Some((pending.intent_id?, pending.event_id)))
        else {
            return;
        };
        let Ok(lanes) = self.resolver.store().recover_outbox_lanes(intent_id) else {
            return;
        };
        if lanes.is_empty()
            || lanes
                .iter()
                .any(|lane| !matches!(lane.state, LaneState::Terminal { .. }))
        {
            return;
        }
        let Ok(CloseIntentOutcome::Closed | CloseIntentOutcome::AlreadyClosed) =
            self.resolver.store_mut().close_terminal_intent(intent_id)
        else {
            return;
        };
        if let Some(pending) = self.pending.remove(&id) {
            self.forget_pending_indexes(id, &pending);
        }
        if let Some(event_id) = event_id {
            if let Some(receipts) = self.event_to_receipts.get_mut(&event_id) {
                receipts.remove(&id);
                if receipts.is_empty() {
                    self.event_to_receipts.remove(&event_id);
                }
            }
        }
    }

    #[cfg(test)]
    fn set_next_attempt_correlation_for_test(&mut self, next: Option<u64>) {
        self.next_attempt_correlation = next;
    }

    /// Consume the one, ever, typed transport handoff for an exact persisted
    /// lane ordinal. The next lane fact commits before any receipt claim or
    /// subsequent wire effect: transport never becomes a second retry owner.
    fn on_event_handoff(
        &mut self,
        correlation: AttemptCorrelation,
        result: HandoffResult,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(target) = self.attempt_correlations.remove(&correlation) else {
            return effects;
        };

        let Some((intent_id, ordinal)) = target.lane else {
            return effects;
        };

        let key = LaneKey {
            intent_id,
            relay: target.session.relay.clone(),
        };
        let Ok(Some(lane)) = self
            .resolver
            .store()
            .recover_outbox_lanes(intent_id)
            .map(|lanes| lanes.into_iter().find(|lane| lane.key == key))
        else {
            return effects;
        };
        if !matches!(
            lane.state,
            LaneState::InFlight {
                ordinal: current,
                phase: InFlightPhase::AwaitingHandoff,
            } if current == ordinal
        ) {
            return effects;
        }

        let durability = self.pending.get(&target.receipt).map(|p| p.durability);
        let detail = AttemptHandoffDetail {
            at: self.clock,
            result: match result {
                HandoffResult::NotHandedOff => HandoffEvidence::NotHandedOff,
                HandoffResult::Written => HandoffEvidence::Written,
                HandoffResult::Ambiguous => HandoffEvidence::Ambiguous,
            },
        };
        let next = match (result, durability) {
            (HandoffResult::NotHandedOff, _) => PostHandoffState::WaitingConnection,
            (HandoffResult::Written, _) | (HandoffResult::Ambiguous, Some(Durability::Durable)) => {
                PostHandoffState::AwaitingAck {
                    deadline: self.clock + ACK_TIMEOUT_SECS,
                }
            }
            (HandoffResult::Ambiguous, Some(Durability::AtMostOnce)) => {
                PostHandoffState::Terminal {
                    outcome: AttemptOutcome::OutcomeUnknown,
                    finished_at: self.clock,
                }
            }
            (HandoffResult::Ambiguous, _) => return effects,
        };
        if self
            .resolver
            .store_mut()
            .record_lane_handoff(&key, lane.revision, ordinal, detail, next)
            .is_err()
        {
            return effects;
        }

        match (result, durability) {
            (HandoffResult::Written, _) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::Sent {
                        relay: target.session.relay,
                        attempt: ordinal,
                        written_at: self.clock,
                    },
                    &mut effects,
                );
            }
            (HandoffResult::Ambiguous, Some(Durability::AtMostOnce)) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::HandoffAmbiguous {
                        relay: target.session.relay.clone(),
                        attempt: ordinal,
                        observed_at: self.clock,
                    },
                    &mut effects,
                );
                self.remove_active_lane(target.receipt, &target.session.relay);
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::OutcomeUnknown(target.session.relay),
                    &mut effects,
                );
                self.close_if_all_lanes_terminal(target.receipt);
            }
            (HandoffResult::NotHandedOff, _) => {
                self.remove_active_lane(target.receipt, &target.session.relay);
                self.connected_relays.remove(&target.session);
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::AwaitingRelay {
                        relay: target.session.relay.clone(),
                    },
                    &mut effects,
                );
                effects.push(Effect::EnsureRelay(target.session));
            }
            (HandoffResult::Ambiguous, Some(Durability::Durable)) => {
                self.emit_write_status(
                    target.receipt,
                    WriteStatus::HandoffAmbiguous {
                        relay: target.session.relay,
                        attempt: ordinal,
                        observed_at: self.clock,
                    },
                    &mut effects,
                );
            }
            (HandoffResult::Ambiguous, _) => {}
        }
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// Full O(pending) re-read of every outstanding write's lanes. This
    /// remains a deliberate architectural stance for `schedule_ready` (its
    /// caller below) and `required_relay_workers`, NOT an oversight (epic
    /// #507 finding E5): both compute durable-cap/attempt-ordinal
    /// accounting, which is defined over ALL outstanding lanes globally --
    /// there is no per-relay narrowing that preserves that meaning, so they
    /// are left unchanged here. `wake_relay_lanes` is the one caller this
    /// full scan was NOT inherent to (a single relay event only ever needs
    /// that relay's own lanes); it now goes through the narrower
    /// `receipts_by_lane_relay` index instead, except in the degraded
    /// fallback which still calls this exact function.
    fn recover_all_lanes(&self) -> Result<Vec<(ReceiptId, RecoveredLane)>, PersistenceError> {
        let mut lanes = Vec::new();
        for (id, pending) in &self.pending {
            let Some(intent_id) = pending.intent_id else {
                continue;
            };
            lanes.extend(
                self.resolver
                    .store()
                    .recover_outbox_lanes(intent_id)?
                    .into_iter()
                    .map(|lane| (*id, lane)),
            );
        }
        lanes.sort_by(|(_, left), (_, right)| left.key.cmp(&right.key));
        Ok(lanes)
    }

    /// Exact relay-SESSION worker demand owned by the reducer right now:
    /// current read-plan sessions plus every nonterminal write lane and every
    /// correlated ephemeral handoff (both as their identity-scoped
    /// `Nip42(signing pubkey)` sessions — #8: a write never rides the Public
    /// read session). The runtime uses this set to release obsolete pool
    /// workers before dispatching replacement wire work, so a finite cap
    /// bounds live work without turning historical read connections into
    /// permanent slot owners.
    ///
    /// A store read failure returns `None`. In that case the runtime retains
    /// every worker rather than risking eviction of a durable lane whose
    /// persisted state could not be inspected.
    pub(crate) fn required_relay_workers(&self) -> Option<BTreeSet<RelaySessionKey>> {
        let mut required: BTreeSet<RelaySessionKey> =
            self.router.plan().reqs.keys().cloned().collect();

        required.extend(
            self.attempt_correlations
                .values()
                .map(|target| target.session.clone()),
        );

        for pending in self.pending.values() {
            let access = AccessContext::Nip42(pending.signing_pubkey);
            required.extend(
                pending
                    .pending_relays
                    .iter()
                    .chain(&pending.unstarted_relays)
                    .chain(&pending.route_blocked_relays)
                    .cloned()
                    .map(|relay| RelaySessionKey::new(relay, access)),
            );

            let Some(intent_id) = pending.intent_id else {
                continue;
            };
            let lanes = self.resolver.store().recover_outbox_lanes(intent_id).ok()?;
            required.extend(lanes.into_iter().filter_map(|lane| {
                (!matches!(lane.state, LaneState::Terminal { .. }))
                    .then_some(RelaySessionKey::new(lane.key.relay, access))
            }));
        }

        Some(required)
    }

    /// The only path that allocates durable attempt ordinals. Eligibility is
    /// persisted first; this reducer then applies stable ordering and the
    /// ratified 32-global/1-per-relay caps before committing Started.
    fn schedule_ready(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Ok(lanes) = self.recover_all_lanes() else {
            self.retry_scheduler_blocked = true;
            return effects;
        };

        let mut in_flight_relays = BTreeSet::new();
        let mut in_flight = 0usize;
        let mut eligible = Vec::new();
        for (id, lane) in lanes {
            match lane.state {
                LaneState::InFlight { .. } | LaneState::LegacyInFlight { .. } => {
                    in_flight = in_flight.saturating_add(1);
                    in_flight_relays.insert(lane.key.relay.clone());
                }
                LaneState::Eligible { since } => eligible.push((since, id, lane)),
                _ => {}
            }
        }
        eligible.sort_by(|(at_a, _, lane_a), (at_b, _, lane_b)| {
            at_a.cmp(at_b).then_with(|| lane_a.key.cmp(&lane_b.key))
        });

        for (_, id, lane) in eligible {
            // The write plane's connectivity check is against the lane's
            // identity-scoped authenticated session (#8 U2: a write rides
            // `Nip42(signing pubkey)`, never the relay's Public read
            // session). A lane whose receipt has no live pending entry has
            // nothing to schedule.
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let session = RelaySessionKey::new(
                lane.key.relay.clone(),
                AccessContext::Nip42(pending.signing_pubkey),
            );
            if !self.connected_relays.contains(&session) {
                if self
                    .resolver
                    .store_mut()
                    .set_lane_waiting(&lane.key, lane.revision, false)
                    .is_ok()
                {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingRelay {
                            relay: lane.key.relay.clone(),
                        },
                        &mut effects,
                    );
                    effects.push(Effect::EnsureRelay(session));
                } else {
                    self.retry_scheduler_blocked = true;
                }
                continue;
            }
            // The AUTH gate: a lane parks before an attempt ordinal is
            // allocated while (a) this exact generation's bounded initial
            // AUTH-discovery observation is still pending, or (b) the relay
            // has actually REQUIRED auth for this session (challenge,
            // auth-required write ack, or restricted close — all of which
            // insert `auth_required_sessions`) and the exact current
            // generation has not completed AUTH. An unchallenged ordinary
            // relay proceeds after its probe releases: a relay that never
            // challenges must not wedge every write, and one that only
            // reveals auth-requirement via `OK false auth-required:` still
            // parks through `handle_write_ack`'s `RelayAckClass::WaitingAuth`
            // path.
            if self.auth_probe_sessions.contains_key(&session)
                || (self.auth_required_sessions.contains(&session)
                    && !self.auth_ready_sessions.contains_key(&session))
            {
                if self
                    .resolver
                    .store_mut()
                    .set_lane_waiting(&lane.key, lane.revision, true)
                    .is_ok()
                {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingAuth {
                            relay: lane.key.relay.clone(),
                        },
                        &mut effects,
                    );
                } else {
                    self.retry_scheduler_blocked = true;
                }
                continue;
            }
            if in_flight >= MAX_GLOBAL_ATTEMPTS || in_flight_relays.contains(&lane.key.relay) {
                continue;
            }
            let Some(event) = self.pending.get(&id).map(|pending| pending.frozen.clone()) else {
                continue;
            };
            let Ok(correlation) = self.alloc_attempt_correlation() else {
                continue;
            };
            let (attempt, advanced) = match self.resolver.store_mut().start_lane_attempt(
                &lane.key,
                lane.revision,
                event.clone(),
                now,
            ) {
                Ok(result) => result,
                Err(_) => {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.unstarted_relays.insert(lane.key.relay.clone());
                    }
                    self.emit_write_status(
                        id,
                        WriteStatus::PersistenceBlocked(lane.key.relay),
                        &mut effects,
                    );
                    continue;
                }
            };
            debug_assert_eq!(
                advanced.state,
                LaneState::InFlight {
                    ordinal: attempt.ordinal,
                    phase: InFlightPhase::AwaitingHandoff,
                }
            );
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.unstarted_relays.remove(&lane.key.relay);
                pending.pending_relays.insert(lane.key.relay.clone());
                pending
                    .attempt_ordinals
                    .insert(lane.key.relay.clone(), attempt.ordinal);
            }
            self.event_to_receipts
                .entry(event.id)
                .or_default()
                .insert(id);
            self.attempt_correlations.insert(
                correlation,
                AttemptCorrelationTarget {
                    receipt: id,
                    session: session.clone(),
                    lane: Some((lane.key.intent_id, attempt.ordinal)),
                },
            );
            effects.push(Effect::PublishEvent(session, event, correlation));
            in_flight += 1;
            in_flight_relays.insert(lane.key.relay);
        }
        effects
    }

    /// Wake every `WaitingConnection` (or, if `auth_only`, `WaitingAuth`)
    /// lane on `session` -- called on every relay connect/disconnect/auth
    /// event. Before epic #507 finding E5, this ran `recover_all_lanes` (a
    /// full `O(pending)` store re-read) and then filtered down to one
    /// relay, TWICE over per event (once here, once again inside
    /// `schedule_ready` at the end). The non-degraded path below instead
    /// narrows via `receipts_by_lane_relay` to exactly the receipts that
    /// actually own a lane on `session.relay`, re-reading only those
    /// intents. (`receipts_by_lane_relay`/`LaneKey` stay URL-keyed in the
    /// store — only the SESSION comparison below, derived per lane from its
    /// pending write's signing identity, decides whether a lane belongs to
    /// THIS session.)
    ///
    /// While `lane_relay_index_degraded`, this falls back to the OLD full
    /// scan, unchanged: the index cannot be trusted to be a superset of
    /// live lanes right now, and guessing wrong here means a lane never
    /// wakes -- a permanently wedged durable write, the worst bug class in
    /// this codebase (see the idle-barrier missed-wakeup fix, d755f39, and
    /// #507's own missed-wakeup finding). A missed wakeup is never an
    /// acceptable price for narrower reads.
    fn wake_relay_lanes(&mut self, session: &RelaySessionKey, auth_only: bool) -> Vec<Effect> {
        let mut effects = Vec::new();

        if self.lane_relay_index_degraded {
            let Ok(lanes) = self.recover_all_lanes() else {
                self.retry_scheduler_blocked = true;
                return effects;
            };
            self.apply_relay_wake(session, auth_only, lanes, &mut effects);
            effects.extend(self.schedule_ready(self.clock));
            return effects;
        }

        // Clone the candidate receipt set first: the loop below needs a
        // mutable borrow of `self` (store reads, `retry_scheduler_blocked`),
        // so it cannot hold a live borrow of `self.receipts_by_lane_relay`
        // at the same time.
        let candidates: Vec<ReceiptId> = self
            .receipts_by_lane_relay
            .get(&session.relay)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .collect();

        let mut lanes: Vec<(ReceiptId, RecoveredLane)> = Vec::new();
        for id in candidates {
            let Some(intent_id) = self.pending.get(&id).and_then(|pending| pending.intent_id)
            else {
                continue;
            };
            match self.resolver.store().recover_outbox_lanes(intent_id) {
                Ok(recovered) => lanes.extend(
                    recovered
                        .into_iter()
                        .filter(|lane| lane.key.relay == session.relay)
                        .map(|lane| (id, lane)),
                ),
                Err(_) => {
                    // A transient read failure for this one receipt, not an
                    // indexing gap -- the established `retry_scheduler_blocked`
                    // idiom (a later engine message retries) applies exactly
                    // as it does everywhere else this door is read, without
                    // needing to distrust the whole index.
                    self.retry_scheduler_blocked = true;
                }
            }
        }
        // Same deterministic order `recover_all_lanes` produces (by
        // `lane.key`): order affects effect emission order, and this must be
        // indistinguishable from the old full-scan behavior for a given
        // input, not merely equivalent in aggregate.
        lanes.sort_by(|(_, left), (_, right)| left.key.cmp(&right.key));

        self.apply_relay_wake(session, auth_only, lanes, &mut effects);
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// The exact per-lane wake body `wake_relay_lanes` ran inline before
    /// epic #507 finding E5, shared now by both its indexed fast path and
    /// its degraded full-scan fallback so the two are behaviorally
    /// identical for a given input. `lanes` is assumed pre-sorted by
    /// `lane.key` (both callers already do this); it need NOT be pre-
    /// filtered to `session` -- the loop below still filters, since the
    /// degraded fallback hands it every pending intent's lanes unfiltered
    /// (exactly as the old, pre-#507 `wake_relay_lanes` body did). A lane
    /// whose receipt has no pending entry is skipped: without a live pending
    /// write there is nothing to wake. Since the AUTH-reducer wave (#8 U2)
    /// the write plane rides the lane's identity-scoped authenticated
    /// session, so a lane belongs to `RelaySessionKey::new(lane.key.relay,
    /// Nip42(pending.signing_pubkey))`.
    fn apply_relay_wake(
        &mut self,
        session: &RelaySessionKey,
        auth_only: bool,
        lanes: Vec<(ReceiptId, RecoveredLane)>,
        effects: &mut Vec<Effect>,
    ) {
        for (id, lane) in lanes {
            let Some(signing_pubkey) = self.pending.get(&id).map(|pending| pending.signing_pubkey)
            else {
                continue;
            };
            if RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey))
                != *session
            {
                continue;
            }
            let should_wake = if auth_only {
                matches!(lane.state, LaneState::WaitingAuth)
            } else {
                matches!(lane.state, LaneState::WaitingConnection)
            };
            if !should_wake {
                continue;
            }
            if self
                .resolver
                .store_mut()
                .set_lane_eligible(&lane.key, lane.revision, self.clock)
                .is_err()
            {
                self.retry_scheduler_blocked = true;
            } else if lane.last_ordinal > 0 {
                self.emit_write_status(
                    id,
                    WriteStatus::RetryEligible {
                        relay: lane.key.relay,
                        attempt: lane.last_ordinal,
                        eligible_at: self.clock,
                    },
                    effects,
                );
            }
        }
    }

    fn consume_due_outbox_deadlines(&mut self, now: Timestamp) -> Vec<Effect> {
        let mut effects = Vec::new();
        loop {
            let due = match self
                .resolver
                .store()
                .due_outbox_deadlines(now, DEADLINE_READ_BATCH)
            {
                Ok(due) => due,
                Err(_) => {
                    self.retry_scheduler_blocked = true;
                    break;
                }
            };
            if due.is_empty() {
                break;
            }
            for deadline in due {
                let id = self.receipt_for_intent(deadline.key.intent_id);
                let lane = self
                    .resolver
                    .store()
                    .recover_outbox_lanes(deadline.key.intent_id)
                    .ok()
                    .and_then(|lanes| {
                        lanes.into_iter().find(|lane| {
                            lane.key == deadline.key && lane.revision == deadline.lane_revision
                        })
                    });
                let Some(lane) = lane else {
                    self.retry_scheduler_blocked = true;
                    continue;
                };
                match (deadline.kind, lane.state.clone()) {
                    (DeadlineKind::RetryEligible, LaneState::Transient { .. }) => {
                        if self
                            .resolver
                            .store_mut()
                            .set_lane_eligible(&lane.key, lane.revision, deadline.at)
                            .is_err()
                        {
                            self.retry_scheduler_blocked = true;
                        }
                    }
                    (
                        DeadlineKind::AckTimeout,
                        LaneState::InFlight {
                            ordinal,
                            phase: InFlightPhase::AwaitingAck { .. },
                        },
                    ) => {
                        let durability =
                            id.and_then(|id| self.pending.get(&id).map(|p| p.durability));
                        if durability == Some(Durability::AtMostOnce) {
                            if self
                                .resolver
                                .store_mut()
                                .finish_lane_attempt(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    AttemptOutcome::OutcomeUnknown,
                                    now,
                                )
                                .is_ok()
                            {
                                if let Some(id) = id {
                                    self.remove_active_lane(id, &lane.key.relay);
                                    self.emit_write_status(
                                        id,
                                        WriteStatus::OutcomeUnknown(lane.key.relay.clone()),
                                        &mut effects,
                                    );
                                    self.close_if_all_lanes_terminal(id);
                                }
                            } else {
                                self.retry_scheduler_blocked = true;
                            }
                        } else {
                            let eligible_at = now + retry_delay_secs(&lane.key, ordinal);
                            if self
                                .resolver
                                .store_mut()
                                .set_lane_transient(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    eligible_at,
                                    TransientCause::AckTimeout,
                                    Some("ack timeout".to_string()),
                                )
                                .is_ok()
                            {
                                if let Some(id) = id {
                                    self.remove_active_lane(id, &lane.key.relay);
                                    self.emit_write_status(
                                        id,
                                        WriteStatus::RetryEligible {
                                            relay: lane.key.relay.clone(),
                                            attempt: ordinal,
                                            eligible_at,
                                        },
                                        &mut effects,
                                    );
                                }
                            } else {
                                self.retry_scheduler_blocked = true;
                            }
                        }
                    }
                    _ => self.retry_scheduler_blocked = true,
                }
            }
            if self.retry_scheduler_blocked {
                break;
            }
        }
        effects.extend(self.schedule_ready(now));
        effects
    }

    /// Rebuild volatile ownership from the journal without reinserting a
    /// single row. Called exactly once by the runtime before its first
    /// command. Retry clocks are reconstructed only from persisted lane facts.
    pub fn recover_on_boot(&mut self) -> Vec<Effect> {
        let recovered = self.resolver.store().recover_outbox();
        let mut effects = Vec::new();
        let mut recovered_ids = Vec::new();
        // This is the one deterministic, from-scratch rebuild of `pending`
        // (and, with it, every index derived from `pending`) -- the exact
        // moment `receipts_by_lane_relay` can be trusted again regardless of
        // what happened in a prior process (epic #507 finding E5).
        self.lane_relay_index_degraded = false;

        for intent in recovered {
            if intent.frozen.kind == nostr::Kind::Authentication {
                let id = ReceiptId(intent.receipt_id);
                let reason = "recovered kind:22242 ordinary write quarantined from AUTH ownership"
                    .to_string();
                self.quarantined_auth_receipts.insert(id, reason.clone());
                effects.push(Effect::EmitReceipt(id, WriteStatus::Failed(reason)));
                continue;
            }
            let parsed_routing = Self::parse_routing_snapshot(&intent.routing);
            let routing_valid = parsed_routing.is_some();
            let routing = parsed_routing.unwrap_or_else(|| {
                WriteRouting::PrivateNarrow(PrivateRoute {
                    relays: NarrowOnly::new(Vec::<RelayUrl>::new()),
                })
            });
            let id = ReceiptId(intent.receipt_id);
            let durability = match intent.durability {
                WriteDurability::Durable => Durability::Durable,
                WriteDurability::AtMostOnce => Durability::AtMostOnce,
            };
            let already_signed = intent.sig_state == IntentSigState::Signed;
            self.pending.insert(
                id,
                PendingWrite {
                    durability,
                    routing,
                    routing_valid,
                    sinks: Vec::new(),
                    intent_id: Some(intent.intent_id),
                    signing_pubkey: intent.expected_pubkey,
                    frozen: intent.frozen.clone(),
                    already_signed,
                    sign_request_in_flight: false,
                    sign_generation: 0,
                    event_id: already_signed.then_some(intent.frozen.id),
                    pending_relays: BTreeSet::new(),
                    unstarted_relays: BTreeSet::new(),
                    route_blocked_relays: BTreeSet::new(),
                    attempt_ordinals: BTreeMap::new(),
                    lane_relays: BTreeSet::new(),
                },
            );
            self.intent_receipts.insert(intent.intent_id, id);
            recovered_ids.push(id);

            if !already_signed {
                continue;
            }
            self.event_to_receipts
                .entry(intent.frozen.id)
                .or_default()
                .insert(id);

            let revisions = match self
                .resolver
                .store()
                .recover_route_revisions(intent.intent_id)
            {
                Ok(revisions) => revisions,
                Err(_) => {
                    // This intent may already own real persisted lanes from
                    // before this boot; skipping straight to the next intent
                    // (as below) means `bootstrap_outbox_lanes` never runs
                    // for it this boot, so the reverse index can never learn
                    // those lanes -- an unprovable gap, so degrade rather
                    // than silently under-index (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    continue;
                }
            };
            let durable_relays = revisions
                .iter()
                .flat_map(|revision| revision.relays.iter().cloned())
                .collect::<BTreeSet<_>>();

            if routing_valid {
                let current_routes = self
                    .resolve_routes(&self.pending[&id].routing, &intent.frozen.pubkey.to_hex())
                    .unwrap_or_default();
                let new_routes = current_routes
                    .difference(&durable_relays)
                    .cloned()
                    .collect::<BTreeSet<_>>();
                if !new_routes.is_empty()
                    && self
                        .resolver
                        .store_mut()
                        .record_route_revision(intent.intent_id, current_routes)
                        .is_err()
                {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.route_blocked_relays.extend(new_routes);
                    }
                }
            }

            let lanes = match self
                .resolver
                .store_mut()
                .bootstrap_outbox_lanes(intent.intent_id)
            {
                Ok(lanes) => lanes,
                Err(_) => {
                    // Same reasoning as the `recover_route_revisions` error
                    // above: this is the sole call that teaches the reverse
                    // index this intent's lanes, so a failure here is an
                    // audit hole, not a "no lanes" fact -- degrade rather
                    // than guess (epic #507 finding E5).
                    self.lane_relay_index_degraded = true;
                    continue;
                }
            };
            for lane in lanes {
                let relay = lane.key.relay.clone();
                // The recovered write lane's worker demand is the intent's
                // identity-scoped authenticated session (#8 U2); recovery
                // redials exactly the session the lane will publish on. The
                // signing identity was frozen at acceptance
                // (`intent.expected_pubkey`), never re-read from the mutable
                // active account.
                let session = RelaySessionKey::new(
                    lane.key.relay.clone(),
                    AccessContext::Nip42(intent.expected_pubkey),
                );
                if let Some(pending) = self.pending.get_mut(&id) {
                    if pending.lane_relays.insert(relay.clone()) {
                        self.receipts_by_lane_relay
                            .entry(relay)
                            .or_default()
                            .insert(id);
                    }
                }
                match lane.state {
                    LaneState::LegacyInFlight { ordinal }
                    | LaneState::InFlight {
                        ordinal,
                        phase: InFlightPhase::AwaitingHandoff,
                    } => match durability {
                        Durability::Durable => {
                            let eligible_at = self.clock;
                            let _ = self.resolver.store_mut().set_lane_transient(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                eligible_at,
                                TransientCause::Interrupted,
                                Some("process restarted before handoff resolved".to_string()),
                            );
                        }
                        Durability::AtMostOnce => {
                            if self
                                .resolver
                                .store_mut()
                                .finish_lane_attempt(
                                    &lane.key,
                                    lane.revision,
                                    ordinal,
                                    AttemptOutcome::OutcomeUnknown,
                                    self.clock,
                                )
                                .is_ok()
                            {
                                effects.push(Effect::EmitReceipt(
                                    id,
                                    WriteStatus::OutcomeUnknown(lane.key.relay),
                                ));
                            }
                        }
                        Durability::Ephemeral => unreachable!(),
                    },
                    LaneState::WaitingConnection
                    | LaneState::Eligible { .. }
                    | LaneState::Transient { .. } => {
                        effects.push(Effect::EnsureRelay(session));
                    }
                    LaneState::InFlight {
                        phase: InFlightPhase::AwaitingAck { .. },
                        ..
                    } => {
                        effects.push(Effect::EnsureRelay(session));
                    }
                    LaneState::WaitingAuth => {
                        // A `WaitingAuth` park never survives a restart: its
                        // authenticated grant was generation-scoped to a socket
                        // this process no longer holds. Recover it as
                        // `WaitingConnection` so the post-connect
                        // `wake_relay_lanes(.., auth_only=false)` re-drives it;
                        // leaving it `WaitingAuth` would strand it forever
                        // (its only wake, `finish_auth_ok`, needs a fresh
                        // client-provoked challenge that boot alone can't cause).
                        // Fail-safe like the disconnect arm: a swallowed reset
                        // failure would silently re-strand the lane — exactly
                        // the missed-wakeup class this guards — so on error mark
                        // recovery degraded (this function's own untrustworthy-
                        // recovery signal) rather than warm a connection that
                        // cannot wake a still-`WaitingAuth` lane.
                        if self
                            .resolver
                            .store_mut()
                            .set_lane_waiting(&lane.key, lane.revision, false)
                            .is_ok()
                        {
                            effects.push(Effect::EnsureRelay(session));
                        } else {
                            self.lane_relay_index_degraded = true;
                        }
                    }
                    LaneState::Terminal { .. } => {}
                }
            }
        }

        self.retry_scheduler_blocked = false;
        effects.extend(self.consume_due_outbox_deadlines(self.clock));
        for id in recovered_ids {
            self.close_if_all_lanes_terminal(id);
        }
        effects
    }
    /// its retained facts. Unknown ids do not create state.
    pub fn reattach_receipt(
        &mut self,
        id: ReceiptId,
        sink: Box<dyn ReceiptSink>,
    ) -> ReattachOutcome {
        if self.quarantined_auth_receipts.contains_key(&id) {
            return ReattachOutcome::RetainedButUnreadable;
        }
        let receipt = match self.resolver.store().reattach_receipt(id.0) {
            Ok(Some(receipt)) => receipt,
            Ok(None) => return ReattachOutcome::NotFound,
            Err(_) => return ReattachOutcome::RetainedButUnreadable,
        };
        if self
            .pending
            .get(&id)
            .is_some_and(|pending| !pending.routing_valid)
        {
            // Boot retained the obligation but could not interpret its
            // frozen routing policy. Replaying even the readable receipt
            // prefix would falsely imply that this observer is attached to
            // actionable live work, and registering it would leak later
            // signer facts from an obligation whose destination is unknown.
            return ReattachOutcome::RetainedButUnreadable;
        }
        let (attempts, details, lanes) = match receipt.intent_id {
            Some(intent_id) => {
                let attempts = match self.resolver.store().recover_attempts(intent_id) {
                    Ok(attempts) => attempts,
                    Err(_) => return ReattachOutcome::RetainedButUnreadable,
                };
                let details = match self.resolver.store().recover_attempt_details(intent_id) {
                    Ok(details) => details,
                    Err(_) => return ReattachOutcome::RetainedButUnreadable,
                };
                let lanes = match self.resolver.store().recover_outbox_lanes(intent_id) {
                    Ok(lanes) => lanes,
                    Err(_) => return ReattachOutcome::RetainedButUnreadable,
                };
                if self
                    .resolver
                    .store()
                    .recover_route_revisions(intent_id)
                    .is_err()
                {
                    return ReattachOutcome::RetainedButUnreadable;
                }
                (attempts, details, lanes)
            }
            None => (Vec::new(), Vec::new(), Vec::new()),
        };
        let status = match receipt.state {
            ReceiptState::Accepted => WriteStatus::Accepted,
            ReceiptState::Signed => WriteStatus::Signed(receipt.frozen_id),
            ReceiptState::Compensated => WriteStatus::Failed("write compensated".to_string()),
            ReceiptState::Abandoned => {
                WriteStatus::Failed("ephemeral write abandoned after restart".to_string())
            }
        };
        let mut replay = vec![status];
        if receipt.state == ReceiptState::Accepted
            && self
                .pending
                .get(&id)
                .is_some_and(|pending| !pending.already_signed)
        {
            replay.push(WriteStatus::AwaitingCapability);
        }
        if receipt.intent_id.is_some() {
            let mut details_by_attempt = details
                .into_iter()
                .map(|detail| ((detail.relay.clone(), detail.ordinal), detail))
                .collect::<BTreeMap<_, _>>();
            let mut awaiting_relay = BTreeSet::new();
            let mut awaiting_auth = BTreeSet::new();
            let mut retry_eligible = BTreeSet::new();
            for attempt in attempts {
                if let Some(detail) =
                    details_by_attempt.remove(&(attempt.relay.clone(), attempt.ordinal))
                {
                    if let Some(handoff) = detail.handoff {
                        match handoff.result {
                            HandoffEvidence::NotHandedOff => {
                                awaiting_relay.insert((attempt.relay.clone(), attempt.ordinal));
                                replay.push(WriteStatus::AwaitingRelay {
                                    relay: attempt.relay.clone(),
                                });
                            }
                            HandoffEvidence::Written => replay.push(WriteStatus::Sent {
                                relay: attempt.relay.clone(),
                                attempt: attempt.ordinal,
                                written_at: handoff.at,
                            }),
                            HandoffEvidence::Ambiguous => {
                                replay.push(WriteStatus::HandoffAmbiguous {
                                    relay: attempt.relay.clone(),
                                    attempt: attempt.ordinal,
                                    observed_at: handoff.at,
                                });
                            }
                        }
                    }
                    if let Some(transient) = detail.transient {
                        if transient.cause == TransientCause::AuthRequired {
                            awaiting_auth.insert((attempt.relay.clone(), attempt.ordinal));
                            replay.push(WriteStatus::AwaitingAuth {
                                relay: attempt.relay.clone(),
                            });
                        } else {
                            retry_eligible.insert((
                                attempt.relay.clone(),
                                attempt.ordinal,
                                transient.eligible_at,
                            ));
                            replay.push(WriteStatus::RetryEligible {
                                relay: attempt.relay.clone(),
                                attempt: attempt.ordinal,
                                eligible_at: transient.eligible_at,
                            });
                        }
                    }
                }
                let status = match attempt.outcome {
                    // Started is only the crash-safe pre-wire fact. #93
                    // deliberately moved Sent to the later transport
                    // Written result, so replaying Started as Sent would
                    // recreate the exact false claim this seam removes.
                    AttemptOutcome::Started => continue,
                    AttemptOutcome::Acked => WriteStatus::Acked(attempt.relay),
                    AttemptOutcome::Rejected(reason) => {
                        WriteStatus::Rejected(attempt.relay, reason)
                    }
                    AttemptOutcome::GaveUp => WriteStatus::GaveUp(attempt.relay),
                    AttemptOutcome::OutcomeUnknown => WriteStatus::OutcomeUnknown(attempt.relay),
                };
                replay.push(status);
            }
            if !details_by_attempt.is_empty() {
                return ReattachOutcome::RetainedButUnreadable;
            }
            for lane in lanes {
                match lane.state {
                    LaneState::WaitingConnection
                        if !awaiting_relay
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push(WriteStatus::AwaitingRelay {
                            relay: lane.key.relay,
                        });
                    }
                    LaneState::WaitingAuth
                        if !awaiting_auth
                            .contains(&(lane.key.relay.clone(), lane.last_ordinal)) =>
                    {
                        replay.push(WriteStatus::AwaitingAuth {
                            relay: lane.key.relay,
                        });
                    }
                    LaneState::Eligible { since }
                        if lane.last_ordinal > 0
                            && !retry_eligible.contains(&(
                                lane.key.relay.clone(),
                                lane.last_ordinal,
                                since,
                            )) =>
                    {
                        replay.push(WriteStatus::RetryEligible {
                            relay: lane.key.relay,
                            attempt: lane.last_ordinal,
                            eligible_at: since,
                        });
                    }
                    LaneState::Transient {
                        ordinal,
                        eligible_at,
                        cause,
                        ..
                    } if cause != TransientCause::AuthRequired
                        && !retry_eligible.contains(&(
                            lane.key.relay.clone(),
                            ordinal,
                            eligible_at,
                        )) =>
                    {
                        replay.push(WriteStatus::RetryEligible {
                            relay: lane.key.relay,
                            attempt: ordinal,
                            eligible_at,
                        });
                    }
                    _ => {}
                }
            }
        }
        if let Some(pending) = self.pending.get(&id) {
            for relay in &pending.unstarted_relays {
                replay.push(WriteStatus::PersistenceBlocked(relay.clone()));
            }
            for relay in &pending.route_blocked_relays {
                replay.push(WriteStatus::RoutePersistenceBlocked(relay.clone()));
            }
        }
        for status in replay {
            sink.on_status(status);
        }
        if let Some(pending) = self.pending.get_mut(&id) {
            pending.sinks.push(Rc::from(sink));
        }
        ReattachOutcome::Attached
    }

    /// Read-only access to the resolver's current demand (test/diagnostic
    /// convenience — the whole point of a headlessly-testable reducer is
    /// that its state can be inspected directly). Returns the TRUE
    /// `ContextualAtom` set (#118, fixed ahead of #107): #106 kept this
    /// surface `ConcreteFilter`-only, reconstructing context via a static
    /// default -- exact ONLY as long as nothing in production constructs a
    /// non-default `Demand`. #107's `SourceAuthority::Pinned` is the first
    /// production path that does, so a reconstruction would silently
    /// collapse two genuinely-distinct atoms (same selection, different
    /// context) that the resolver correctly tracks as two independent
    /// entries into one. Widened rather than patched with an assertion,
    /// per the repo's no-compat-alias convention -- this mirrors
    /// `nmp_resolver::Engine::active_demand()` exactly.
    pub fn active_demand(&self) -> BTreeSet<ContextualAtom> {
        self.resolver.active_demand()
    }

    /// Read-only coverage introspection (test/diagnostic convenience,
    /// mirroring `active_demand`): the proven interval for `atom`'s
    /// window-erased shape at `relay`, if any coverage has been recorded.
    /// `atom` is the atom's TRUE `ContextualAtom` (#118, fixed ahead of
    /// #107) -- the caller supplies the actual context an atom was
    /// acquired under, never a reconstruction. Before this fix, a
    /// `ConcreteFilter`-only signature reconstructed `source`/`access` via
    /// `Demand::from_filter`'s static default, which was exact only as
    /// long as every production atom took that default path; #107's
    /// `SourceAuthority::Pinned` breaks that assumption; the reconstruction
    /// would then compute the WRONG `CoverageKey` and silently report
    /// "not covered" for coverage that IS actually proven.
    pub fn get_coverage(
        &self,
        atom: &ContextualAtom,
        relay: &RelayUrl,
    ) -> Option<nmp_store::CoverageInterval> {
        self.resolver
            .store()
            .get_coverage(nmp_store::coverage_key(atom), relay)
    }

    /// The engine-global diagnostics projection (M5 plan §1.2 step 2) — "the
    /// acceptance test made visible": combines `nmp_router::Router::
    /// diagnostics()` (per-session wire-sub count, exact filters, lane
    /// counts, reverse coverage) with this reducer's own `events_by_session_
    /// kind` counter and per-(relay, filter) coverage read via
    /// `Self::get_coverage`. Pure and read-only — never influences
    /// routing/delivery; every number here is real state this reducer
    /// already tracks for other reasons, never fabricated/estimated.
    pub fn diagnostics_snapshot(&self) -> DiagnosticsSnapshot {
        let mut snapshot = diagnostics::build(
            self.router.diagnostics(),
            self.router.plan(),
            &self.events_by_session_kind,
            self.discovered_private_relays_rejected,
            |relay, key| self.resolver.store().get_coverage(key, relay),
        );
        // Surface the read-only degrade signal (issue #122) if an ingest/read
        // door has failed — the one persistence-health fact `build` cannot
        // see on its own.
        snapshot.store_degraded = self.store_degraded.clone();
        snapshot.transport_degraded = self
            .relay_open_failures
            .iter()
            .next()
            .map(|(session, reason)| format!("{}: {reason}", session.relay))
            .or_else(|| self.transport_degraded.clone());
        let mut auth_sessions = BTreeMap::new();
        for (handle, session) in self.slot_to_relay.values() {
            if session.access == AccessContext::Public || !self.connected_relays.contains(session) {
                continue;
            }
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    access: session.access,
                    transport_slot: handle.slot,
                    transport_generation: handle.generation,
                    epoch_sequence: None,
                    challenge_hash: None,
                    phase: AuthDiagnosticsPhase::AwaitingChallenge,
                    policy_bound: false,
                    signer_bound: false,
                    auth_event_id: None,
                    send_handoff_accepted: false,
                    relay_ok_accepted: false,
                },
            );
        }
        for (session, state) in &self.auth_sessions {
            let (phase, auth_event_id, send_handoff_accepted, relay_ok_accepted) =
                match &state.phase {
                    AuthSessionPhase::AwaitingPolicy { .. } => {
                        (AuthDiagnosticsPhase::AwaitingPolicy, None, false, false)
                    }
                    AuthSessionPhase::AwaitingSignature { .. } => {
                        (AuthDiagnosticsPhase::AwaitingSignature, None, false, false)
                    }
                    AuthSessionPhase::AwaitingSend { event_id, .. } => (
                        AuthDiagnosticsPhase::AwaitingSend,
                        Some(*event_id),
                        false,
                        false,
                    ),
                    AuthSessionPhase::AwaitingOk { event_id } => (
                        AuthDiagnosticsPhase::AwaitingRelayAck,
                        Some(*event_id),
                        true,
                        false,
                    ),
                    AuthSessionPhase::Ready { event_id } => {
                        (AuthDiagnosticsPhase::Ready, Some(*event_id), true, true)
                    }
                    AuthSessionPhase::Denied => (AuthDiagnosticsPhase::Denied, None, false, false),
                    AuthSessionPhase::Error => (AuthDiagnosticsPhase::Error, None, false, false),
                };
            auth_sessions.insert(
                session.clone(),
                AuthDiagnosticsSnapshot {
                    relay: session.relay.clone(),
                    access: session.access,
                    transport_slot: state.epoch.handle.slot,
                    transport_generation: state.epoch.handle.generation,
                    epoch_sequence: Some(state.epoch.sequence),
                    challenge_hash: (!state.challenge.is_empty()).then(|| {
                        blake3::hash(state.challenge.as_bytes())
                            .to_hex()
                            .to_string()
                    }),
                    phase,
                    policy_bound: state.policy_instance.is_some(),
                    signer_bound: state.signer_instance.is_some(),
                    auth_event_id,
                    send_handoff_accepted,
                    relay_ok_accepted,
                },
            );
        }
        snapshot.auth_sessions = auth_sessions.into_values().collect();
        for relay in &mut snapshot.relays {
            // NIP-11 advertisement and the NIP-77 behavioral probe are both
            // PUBLIC-session evidence (#8): the one-shot HTTP document and
            // the probe run outside/over the unauthenticated session, so a
            // protected session's row must never inherit them — its
            // capability facts stay honestly "unknown".
            if relay.access != AccessContext::Public {
                continue;
            }
            if let Some(information) = self.nip11_information.get(&relay.relay) {
                relay.nip11_supported_nips = information.supported_nips.clone();
                relay.nip11_document_revision = Some(information.document_revision.clone());
                relay.nip11_freshness = Some(if self.clock.as_secs() < information.fresh_until {
                    "fresh"
                } else {
                    "stale"
                });
                relay.nip11_last_error = information.last_error.as_ref().map(ToString::to_string);
            }
            relay.nip77_advertisement = match relay
                .nip11_supported_nips
                .as_ref()
                .map(|nips| nips.contains(&77))
            {
                Some(true) => "advertised_supported",
                Some(false) => "advertised_unsupported",
                None => "unknown",
            };
            relay.nip77_behavior = match self.prober.state(&relay.relay) {
                crate::negentropy::ProbeState::Unknown => "unknown",
                crate::negentropy::ProbeState::Probing => "probing",
                crate::negentropy::ProbeState::Supported => "behaviorally_proven",
                crate::negentropy::ProbeState::Unsupported => "behaviorally_rejected",
            };
        }
        snapshot
    }

    #[cfg(test)]
    pub(crate) fn nip11_information_len(&self) -> usize {
        self.nip11_information.len()
    }

    /// A pure clock update PLUS two deadline sweeps: NIP-40 expiry
    /// (retraction-and-negative-deltas.md §3.2 — drains `store.expire_due`
    /// and retracts every row past its deadline) and the negentropy
    /// liveness-deadline sweep (plan §6 E, harvest `nmp-nip77`'s "30s
    /// liveness-deadline REQ fallback"): any reconciliation session open
    /// longer than [`NEG_LIVENESS_DEADLINE_SECS`] against `now` is
    /// abandoned in favor of a plain REQ for the same (unfloored/unlimited)
    /// filter. The same tick first consumes every due durable-lane retry/ACK
    /// deadline through the one outbox scheduler.
    ///
    /// `runtime::engine_loop` (§3.3, #39) is what actually drives this on
    /// its own now: it arms `cmd_rx.recv_timeout` off [`Self::next_deadline`]
    /// and dispatches `EngineMsg::Tick(wall_now())` exactly when that
    /// timeout elapses (D8: the existing blocking recv grows a timeout,
    /// never a poll-loop timer thread). Both sweeps stay real and unit-
    /// tested here against a synthetic clock regardless of who calls this
    /// -- the runtime driver is a caller, not part of the mechanism.
    pub fn tick(&mut self, now: Timestamp) -> Vec<Effect> {
        self.clock = now;
        let mut effects = Vec::new();
        self.retry_scheduler_blocked = false;
        effects.extend(self.consume_due_outbox_deadlines(now));

        // NIP-40 expiry (retraction-and-negative-deltas.md §3.2). The
        // deadline-armed runtime driver above dispatches this tick at the
        // store's next indexed expiration; this reducer owns the atomic
        // removal and projection reaction.
        // Drain every row whose expiration is due straight through the
        // store's own index (`O(log n + due)`, never a scan), then route
        // the removed rows through the SAME retraction lane a kind:5
        // delete already uses inside `ingest_observed` — `resolver.retract`
        // seeds dirty-marks from `removed` alone, then stable simple handles
        // consume the exact committed removals while demand-changing or
        // complex shapes retain the broad refresh oracle.
        match self.resolver.store_mut().expire_due(now) {
            Ok(expired) if !expired.is_empty() => {
                let removed: Vec<_> = expired.into_iter().map(|se| se.event).collect();
                match self.resolver.retract(removed) {
                    Ok(committed) => {
                        self.apply_committed_mutation(committed, &mut effects);
                    }
                    Err(e) => self.degrade_store(e, &mut effects),
                }
            }
            Ok(_) => {}
            Err(e) => self.degrade_store(e, &mut effects),
        }

        // `>=` against the EXACT `Timestamp` threshold `next_deadline()`
        // arms for (`started_at + NEG_LIVENESS_DEADLINE_SECS`) -- not the
        // `as_secs()`-truncated, strictly-greater subtraction this used to
        // be. Those two must reference the identical expression: the
        // runtime driver's `recv_timeout` wakes AT the deadline it was
        // armed for (`duration_until` floors an already-reached deadline to
        // zero), so a strict `>` here left the sweep still false at that
        // exact `now`, `next_deadline()` still returning the same
        // deadline, and `duration_until` still flooring to zero -- a
        // `recv_timeout(0)` busy-spin until the wall clock ticked over into
        // the NEXT whole second (`as_secs()` finally reading `31 > 30`).
        // `>=` clears the session in the very tick that reaches its
        // deadline, so `next_deadline()` recomputes without it and the loop
        // parks -- see #39's fix-up review and the regression test this
        // predicate exists to satisfy.
        let stale: Vec<SubId> = self
            .neg_sessions
            .iter()
            .filter(|(_, s)| now >= s.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .map(|(id, _)| id.clone())
            .collect();
        for sub_id in stale {
            if let Some(session) = self.neg_sessions.remove(&sub_id) {
                self.neg_session_fallback_to_req(sub_id, session, &mut effects);
            }
        }

        effects
    }

    /// The earliest wall-clock instant at which [`Self::tick`] must run for
    /// something to actually happen (retraction-and-negative-deltas.md
    /// §3.2): the min over every deadline source this reducer currently
    /// tracks -- NIP-40 expiry (`store.next_expiration()`, index-backed) and
    /// open negentropy sessions' liveness deadlines (`started_at +
    /// NEG_LIVENESS_DEADLINE_SECS`). `None` means no timer needs to fire at
    /// all right now: `runtime::engine_loop`'s `recv_timeout` driver (§3.3)
    /// sleeps forever on the plain `recv()` in that case, exactly matching
    /// the doc's "a light embedder with no deadlines pays nothing".
    /// Extensible to future timers (backoff, drop-grace debounce) by folding
    /// another `.min()` term in here -- the runtime driver itself never
    /// needs to change to pick up a new deadline source.
    pub fn next_deadline(&self) -> Option<Timestamp> {
        let expiry = self.resolver.store().next_expiration();
        let neg_liveness = self
            .neg_sessions
            .values()
            .map(|session| session.started_at + NEG_LIVENESS_DEADLINE_SECS)
            .min();
        let outbox = (!self.retry_scheduler_blocked)
            .then(|| self.resolver.store().next_outbox_deadline().ok().flatten())
            .flatten();
        [expiry, neg_liveness, outbox].into_iter().flatten().min()
    }

    pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect> {
        // A prior persistence failure suppresses a due outbox deadline only
        // until real work arrives. Re-expose it after this message so the
        // runtime immediately drives a fresh Tick instead of either spinning
        // on the failed transition or suppressing retry forever.
        self.retry_scheduler_blocked = false;
        let mut effects = match msg {
            EngineMsg::Subscribe(query, sink) => self.on_subscribe(query, sink),
            EngineMsg::Unsubscribe(id) => self.on_unsubscribe(id),
            EngineMsg::SubscribeHistory(query, sink) => self.on_subscribe_history(query, sink),
            EngineMsg::RequestRows(id, at_least) => self.on_request_rows(id, at_least),
            EngineMsg::CommitHistoryLoad(id) => self.on_commit_history_load(id),
            EngineMsg::RollbackHistoryLoad(id) => self.on_rollback_history_load(id),
            EngineMsg::UnsubscribeHistory(id) => self.on_unsubscribe_history(id),
            EngineMsg::SetActivePubkey(pk) => self.on_set_active_pubkey(pk),
            EngineMsg::Publish(intent, sink) => self.on_publish(intent, sink),
            EngineMsg::RelayConnected(handle, session) => self.on_relay_connected(handle, session),
            EngineMsg::AuthProbeReleased(handle, session) => {
                self.on_auth_probe_released(handle, session)
            }
            EngineMsg::RelayInformationResolved(url, information) => {
                self.on_relay_information_resolved(url, information)
            }
            EngineMsg::RelayDisconnected(handle, session, reason) => {
                self.on_relay_disconnected(handle, session, reason)
            }
            EngineMsg::RelayHealth(handle, session, health) => {
                self.on_relay_health(handle, session, health)
            }
            EngineMsg::RelayOpenFailed(session, reason) => {
                if self
                    .required_relay_workers()
                    .is_some_and(|required| required.contains(&session))
                {
                    self.relay_open_failures.insert(session, reason);
                    vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
                } else {
                    Vec::new()
                }
            }
            EngineMsg::RelayFrame(handle, session, frame) => {
                self.on_relay_frame(handle, session, frame)
            }
            EngineMsg::RelayFrames(frames) => self.on_relay_frames(frames),
            EngineMsg::SignerCompleted(id, generation, result) => {
                self.on_signer_completed(id, generation, result)
            }
            EngineMsg::SignerUnavailable(id, generation) => {
                self.on_signer_unavailable(id, generation)
            }
            EngineMsg::SignerAttached(pk) => self.on_signer_attached(pk),
            EngineMsg::AuthPolicyCompleted(token, instance, outcome) => {
                self.on_auth_policy_completed(token, instance, outcome)
            }
            EngineMsg::AuthSignerCompleted(token, instance, outcome) => {
                self.on_auth_signer_completed(token, instance, outcome)
            }
            EngineMsg::AuthCapabilityBound {
                token,
                capability,
                instance,
            } => self.on_auth_capability_bound(token, capability, instance),
            EngineMsg::AuthSendCompleted(token, outcome) => {
                self.on_auth_send_completed(token, outcome)
            }
            EngineMsg::AuthCapabilityInvalidated(pubkey, capability, instance) => {
                self.on_auth_capability_invalidated(pubkey, capability, instance)
            }
            EngineMsg::CancelWrite(id) => {
                let mut effects = self.on_cancel_write(id);
                effects.extend(self.schedule_ready(self.clock));
                effects
            }
            EngineMsg::EventHandoff(correlation, result) => {
                self.on_event_handoff(correlation, result)
            }
            EngineMsg::Tick(now) => self.tick(now),
        };
        if self.prune_unowned_relay_state() {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        effects
    }

    fn prune_unowned_relay_state(&mut self) -> bool {
        // `required_relay_workers()` reads outbox lanes from the store; with
        // nothing to prune it must not tax every reducer message with that
        // scan (the wake-falsifiers in `core_headless.rs` count exactly
        // these reads).
        if self.relay_open_failures.is_empty() && self.auth_required_sessions.is_empty() {
            return false;
        }
        let Some(required) = self.required_relay_workers() else {
            return false;
        };
        let before = self.relay_open_failures.len();
        self.relay_open_failures
            .retain(|session, _| required.contains(session));
        self.auth_required_sessions
            .retain(|session| required.contains(session));
        self.relay_open_failures.len() != before
    }

    fn on_relay_health(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        health: RelayHealth,
    ) -> Vec<Effect> {
        // Health delivery crosses the off-lock sink and may arrive after the
        // slot has reopened for a different generation OR a different
        // session: accept it only when BOTH halves of the reported
        // (handle, session) pair are exactly the slot's current occupant
        // (#8) — health from a slot never seen connected proves nothing.
        let Some((current, current_session)) = self.slot_to_relay.get(&handle.slot) else {
            return Vec::new();
        };
        if *current != handle || *current_session != session {
            return Vec::new();
        }
        self.transport_degraded = health.last_error.or_else(|| {
            (health.invalid_signature_count > 0).then(|| {
                format!(
                    "relay slot {} rejected {} invalid signature frame(s)",
                    handle.slot, health.invalid_signature_count
                )
            })
        });
        vec![Effect::EmitDiagnostics(self.diagnostics_snapshot())]
    }

    // ---- subscribe / unsubscribe / re-root ------------------------------

    fn on_subscribe(&mut self, query: LiveQuery, sink: Box<dyn RowSink>) -> Vec<Effect> {
        let mut effects = Vec::new();
        // Graph construction can read the store (a `Derived` binding resolves
        // its inner query). On a persistence failure (issue #122) degrade to
        // read-only and install NO handle rather than panic — the observer
        // simply receives no rows.
        let (qh, _delta) = match self.resolver.subscribe(query) {
            Ok(v) => v,
            Err(e) => {
                self.degrade_store(e, &mut effects);
                return effects;
            }
        };
        let id = qh.id();
        self.recompile(&mut effects);
        // A new query can change the capped greedy source plan for EVERY
        // existing query, even when their rows are unchanged. Refresh the
        // survivors against the newly-finalized plan before installing the
        // new handle; otherwise their "current-plan" evidence can retain a
        // source that the router just dropped (or omit one it just added).
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        self.handles.insert(
            id,
            HandleState {
                _handle: qh,
                sink,
                last_rows: BTreeMap::new(),
                last_evidence: None,
                projection_complete: false,
            },
        );
        self.refresh_handle(id, &mut effects);
        effects
    }

    fn on_unsubscribe(&mut self, id: HandleId) -> Vec<Effect> {
        let _delta = self.resolver.unsubscribe(id);
        self.handles.remove(&id);
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        // Removing one query can free capped-plan capacity and therefore
        // change the planned sources of every surviving handle.
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects
    }

    fn on_subscribe_history(
        &mut self,
        query: HistoryQuery,
        sink: Box<dyn HistorySink>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let (handle, _) = match self.resolver.subscribe(query.initial_demand()) {
            Ok(value) => value,
            Err(error) => {
                self.degrade_store(error, &mut effects);
                return effects;
            }
        };
        let handle_id = handle.id();
        let id = HistorySessionId(self.next_history_id);
        self.next_history_id = self.next_history_id.wrapping_add(1).max(1);
        self.history_by_handle.insert(handle_id, id);
        self.histories.insert(
            id,
            HistoryState {
                target_rows: query.page_size(),
                query,
                handles: vec![handle],
                handle_ids: BTreeSet::from([handle_id]),
                live_handle_id: handle_id,
                acquisitions: BTreeMap::new(),
                sink,
                acquired_tie_seconds: BTreeSet::new(),
                last_rows: BTreeMap::new(),
                order: BTreeSet::new(),
                last_evidence: None,
                projection_complete: false,
                load: WindowLoad::Idle,
                pending_load: None,
            },
        );

        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories_except(id, &mut effects);
        self.refresh_history(id, WindowLoad::Idle, &mut effects);
        effects
    }

    fn on_unsubscribe_history(&mut self, id: HistorySessionId) -> Vec<Effect> {
        let Some(state) = self.histories.remove(&id) else {
            return Vec::new();
        };
        for handle in state.handles {
            self.history_by_handle.remove(&handle.id());
            let _ = self.resolver.unsubscribe(handle.id());
        }
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects
    }

    /// Declaratively raise this window's row target (#485). Monotonic,
    /// idempotent, and clamped to the declared `max_rows`. Replaces the old
    /// `on_load_older` continuation-token door: there is no token to validate,
    /// no generation to go stale, and no `LoadInProgress`/`AtBound`/
    /// `NoBoundary` error — an in-flight advance simply raises the target, and
    /// being at the bound is a frame fact, not an error.
    fn on_request_rows(&mut self, id: HistorySessionId, at_least: usize) -> Vec<Effect> {
        let Some(state) = self.histories.get(&id) else {
            // The session was withdrawn concurrently. The facade keeps a
            // window's session alive for its whole lifetime, so this is only
            // reachable as a benign teardown race — report Ok, do nothing.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        };
        let max = state.query.max_rows();
        let old_target = state.target_rows;
        let new_target = old_target.max(at_least).min(max);

        // A staged advance is already in flight. This is only reachable when a
        // caller drives `request_rows` between stage and commit (the runtime
        // commits within one command, so between commands there is never a
        // lingering pending load). Raise the target and defer: the post-commit
        // continuation converges the window to it.
        if state.pending_load.is_some() {
            if new_target != old_target {
                self.histories
                    .get_mut(&id)
                    .expect("history remains live")
                    .target_rows = new_target;
            }
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        if new_target == old_target {
            // Raising the target cannot grow the window.
            if old_target == max {
                // At the declared bound: emit exactly one `AtBound` frame beat
                // (a FACT, never an error) through the normal staged
                // EmitHistory path so mailbox conflation applies uniformly.
                return self.stage_history_atbound(id, max);
            }
            // At or below the current target and below the bound: a pure
            // no-op. Any still-unfilled gap converges through the live
            // acquisition and the post-commit continuation, not a re-request.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        // Real growth: raise the target and stage one advance toward it.
        self.stage_history_advance(id, new_target)
    }

    /// The canonical older boundary of one window: its oldest retained row in
    /// NIP-01 newest-first order (`created_at ASC`, then `event_id DESC`).
    /// This is the cursor an advance fetches strictly older than. `None` when
    /// the window holds no rows yet.
    fn window_boundary(&self, id: HistorySessionId) -> Option<nmp_store::EventCursor> {
        let state = self.histories.get(&id)?;
        state
            .last_rows
            .iter()
            .max_by(|(a_id, a), (b_id, b)| {
                nip01_newest_first(
                    (a.event.created_at.as_secs(), a_id),
                    (b.event.created_at.as_secs(), b_id),
                )
            })
            .map(|(event_id, row)| nmp_store::EventCursor::new(row.event.created_at, *event_id))
    }

    /// Stage one bounded advance toward `new_target`, opening the tie-second
    /// and older-range acquisitions for the current boundary and projecting
    /// the newly exposed lower segment as a prospective plan. Nothing becomes
    /// observable until the runtime's synchronous reply receiver accepts
    /// success and commits (`on_commit_history_load`); on any staging failure
    /// the prior projection is restored exactly (`on_rollback_history_load`)
    /// and the collapsed advance error is reported.
    ///
    /// The advance chunk is the actual shortfall (`target - held`), not a
    /// fixed page size, so a single `request_rows(at_least)` asks the wire for
    /// exactly the rows it still needs.
    fn stage_history_advance(&mut self, id: HistorySessionId, new_target: usize) -> Vec<Effect> {
        let mut effects = Vec::new();
        let boundary = self.window_boundary(id);

        let (
            query,
            prior_target,
            prior_load,
            prior_evidence,
            prior_projection_complete,
            needs_tie,
            old_len,
            needed,
        ) = {
            let state = self
                .histories
                .get(&id)
                .expect("advance requires a live session");
            let prior_target = state.target_rows;
            let old_len = state.last_rows.len();
            let effective_target = new_target.max(prior_target);
            let needed = effective_target.saturating_sub(old_len);
            let needs_tie = boundary.as_ref().is_some_and(|cursor| {
                !state
                    .acquired_tie_seconds
                    .contains(&cursor.created_at.as_secs())
            });
            (
                state.query.clone(),
                prior_target,
                state.load,
                state.last_evidence.clone(),
                state.projection_complete,
                needs_tie,
                old_len,
                needed,
            )
        };

        // Raise the target now: `history_rows_and_evidence_for` /
        // `advance_history_projection` both read `target_rows`.
        {
            let state = self.histories.get_mut(&id).expect("history remains live");
            state.target_rows = state.target_rows.max(new_target);
        }

        let Some(boundary) = boundary else {
            // No retained rows: there is no older boundary to fetch behind.
            // The target is raised; the live acquisition and future committed
            // rows fill toward it. Nothing to stage now.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        };
        if needed == 0 {
            // The retained set already satisfies the target (an auto-fill call
            // raced a refresh). Nothing to stage.
            return vec![Effect::HistoryLoadResult(id, Ok(()))];
        }

        {
            let state = self.histories.get_mut(&id).expect("history remains live");
            state.pending_load = Some(PendingHistoryLoad {
                prior_target_rows: prior_target,
                prior_load,
                prior_evidence,
                prior_projection_complete,
                acquired_tie_second: needs_tie.then_some(boundary.created_at.as_secs()),
                opened_handle_ids: Vec::new(),
                added_row_ids: Vec::new(),
                staged_batches: Vec::new(),
            });
        }

        // Each opened acquisition is tagged with its kind for the #486
        // supersede-close: `Some(second)` for the tie-second REQ, `None` for
        // the older-range REQ.
        let mut opened: Vec<(QueryHandle, Option<u64>)> = Vec::new();
        let boundary_second = boundary.created_at.as_secs();
        if needs_tie {
            if let Some(tie) = query.tie_second_demand(boundary_second) {
                match self.resolver.subscribe(tie) {
                    Ok((handle, _)) => opened.push((handle, Some(boundary_second))),
                    Err(error) => {
                        self.degrade_store(error, &mut effects);
                        effects.extend(self.on_rollback_history_load(id));
                        effects.push(Effect::HistoryLoadResult(
                            id,
                            Err(HistoryAdvanceError::StoreUnavailable),
                        ));
                        return effects;
                    }
                }
            }
        }
        if let Some(older) = query.older_demand(boundary_second, needed) {
            match self.resolver.subscribe(older) {
                Ok((handle, _)) => opened.push((handle, None)),
                Err(error) => {
                    for (handle, _) in opened {
                        let _ = self.resolver.unsubscribe(handle.id());
                    }
                    self.degrade_store(error, &mut effects);
                    effects.extend(self.on_rollback_history_load(id));
                    effects.push(Effect::HistoryLoadResult(
                        id,
                        Err(HistoryAdvanceError::StoreUnavailable),
                    ));
                    return effects;
                }
            }
        }

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remains live during synchronous advance");
            if needs_tie {
                state.acquired_tie_seconds.insert(boundary_second);
            }
            for (handle, kind) in opened {
                let handle_id = handle.id();
                state.handle_ids.insert(handle_id);
                state.handles.push(handle);
                state.acquisitions.insert(handle_id, kind);
                self.history_by_handle.insert(handle_id, id);
                state
                    .pending_load
                    .as_mut()
                    .expect("load was staged before opening resolver handles")
                    .opened_handle_ids
                    .push(handle_id);
            }
        }

        // Build the prospective plan without touching live router,
        // attribution, diagnostics, other projections, or any sink.
        let shadow_plan = self.history_shadow_plan();
        let requesting = self.history_batch(id, Vec::new(), WindowLoad::Requesting);
        let added = match self.advance_history_projection(id, boundary, old_len, &shadow_plan) {
            Ok((batch, added)) => {
                let added_row_ids = batch
                    .deltas
                    .iter()
                    .filter_map(|delta| match delta {
                        RowDelta::Added(row) => Some(row.event.id),
                        RowDelta::SourcesGrew { .. } | RowDelta::Removed(_) => None,
                    })
                    .collect();
                let pending = self
                    .histories
                    .get_mut(&id)
                    .expect("history remains live during staged advance")
                    .pending_load
                    .as_mut()
                    .expect("load remains staged until runtime acknowledgement");
                pending.added_row_ids = added_row_ids;
                pending.staged_batches = vec![requesting, batch];
                added
            }
            Err(error) => {
                if let Some(state) = self.histories.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(error, &mut effects);
                effects.extend(self.on_rollback_history_load(id));
                effects.push(Effect::HistoryLoadResult(
                    id,
                    Err(HistoryAdvanceError::StoreUnavailable),
                ));
                return effects;
            }
        };
        debug_assert!(added <= needed);
        effects.push(Effect::PreflightHistoryRelays(
            shadow_plan.reqs.keys().cloned().collect(),
        ));
        effects.push(Effect::HistoryLoadResult(id, Ok(())));
        effects
    }

    /// Stage a single `AtBound { max }` frame beat: the window is already at
    /// its declared ceiling, so `request_rows` cannot grow it, but the caller
    /// still gets one delivered fact. It rides the same staged commit path as
    /// a real advance (empty relay preflight, no opened handles, no target
    /// change) so it conflates identically and rolls back cleanly if the
    /// runtime never accepts it.
    fn stage_history_atbound(&mut self, id: HistorySessionId, max: usize) -> Vec<Effect> {
        let (prior_target, prior_load, prior_evidence, prior_projection_complete) = {
            let state = self.histories.get(&id).expect("history remains live");
            (
                state.target_rows,
                state.load,
                state.last_evidence.clone(),
                state.projection_complete,
            )
        };
        let batch = self.history_batch(id, Vec::new(), WindowLoad::AtBound { max });
        let state = self.histories.get_mut(&id).expect("history remains live");
        state.pending_load = Some(PendingHistoryLoad {
            prior_target_rows: prior_target,
            prior_load,
            prior_evidence,
            prior_projection_complete,
            acquired_tie_second: None,
            opened_handle_ids: Vec::new(),
            added_row_ids: Vec::new(),
            staged_batches: vec![batch],
        });
        vec![
            Effect::PreflightHistoryRelays(BTreeSet::new()),
            Effect::HistoryLoadResult(id, Ok(())),
        ]
    }

    fn on_commit_history_load(&mut self, id: HistorySessionId) -> Vec<Effect> {
        if !self
            .histories
            .get(&id)
            .is_some_and(|state| state.pending_load.is_some())
        {
            return Vec::new();
        }

        // #486: retire the historical tie/older acquisitions the session no
        // longer needs, so a deep scroll of K advances never accumulates O(K)
        // live relay subscriptions. Three classes of handle are KEPT open:
        //   * the permanent live-top demand (`live_handle_id`);
        //   * the advance now committing (its own just-opened handles); and
        //   * the tie-second REQ for the CURRENT window boundary second — a
        //     dense same-second boundary keeps that second as the boundary
        //     across several advances (its `needs_tie` gate stays satisfied
        //     without re-opening), and closing its REQ before the boundary has
        //     descended below it could drop a not-yet-projected same-second
        //     row (the #474 tie-second correctness class). It is retired only
        //     once the boundary moves strictly older, at which point every
        //     in-store row at that second is already projected as interior.
        // Every OTHER acquisition — older-range REQs (always re-requestable, so
        // never a permanent gap) and tie REQs for seconds no longer the
        // boundary — is retired here. `acquired_tie_seconds` is deliberately
        // retained (that is the coverage evidence) so a later advance never
        // re-requests a tie second already covered. The recompile just below
        // re-diffs the demand and emits the wire CLOSEs for the dropped handles.
        let superseded: Vec<HandleId> = {
            let state = self
                .histories
                .get(&id)
                .expect("committed history remained live");
            let current: BTreeSet<HandleId> = state
                .pending_load
                .as_ref()
                .expect("commit checked the staged history load")
                .opened_handle_ids
                .iter()
                .copied()
                .collect();
            let live = state.live_handle_id;
            let boundary_second = self
                .window_boundary(id)
                .map(|cursor| cursor.created_at.as_secs());
            let state = self
                .histories
                .get(&id)
                .expect("committed history remained live");
            state
                .acquisitions
                .iter()
                .filter(|(handle, kind)| {
                    if **handle == live || current.contains(handle) {
                        return false;
                    }
                    // Keep the tie REQ whose second is still the boundary.
                    !matches!((kind, boundary_second), (Some(second), Some(b)) if *second == b)
                })
                .map(|(handle, _)| *handle)
                .collect()
        };
        if !superseded.is_empty() {
            for handle_id in &superseded {
                self.history_by_handle.remove(handle_id);
                let _ = self.resolver.unsubscribe(*handle_id);
            }
            let state = self
                .histories
                .get_mut(&id)
                .expect("committed history remained live");
            state
                .handles
                .retain(|handle| !superseded.contains(&handle.id()));
            for handle_id in &superseded {
                state.handle_ids.remove(handle_id);
                state.acquisitions.remove(handle_id);
            }
        }

        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories_except(id, &mut effects);

        let (made_progress, target, len, has_boundary) = {
            let state = self
                .histories
                .get_mut(&id)
                .expect("committed history remained live");
            let pending = state
                .pending_load
                .take()
                .expect("commit checked the staged history load");
            let made_progress = !pending.added_row_ids.is_empty();
            for batch in pending.staged_batches {
                state.sink.on_history(batch.clone());
                effects.push(Effect::EmitHistory(id, batch));
            }
            (
                made_progress,
                state.target_rows,
                state.last_rows.len(),
                !state.order.is_empty(),
            )
        };

        // Continuation loop (#485): the committed advance made progress but
        // the target is still unmet and an older boundary remains. Stage the
        // next advance automatically, one at a time — the runtime's commit
        // loop drives this to convergence. The `made_progress` guard makes the
        // loop bounded: an advance that adds no canonical row (store exhausted
        // locally; the older-range wire request already placed) does not
        // re-stage, so it never spins waiting on the network.
        if made_progress && target > len && has_boundary {
            effects.extend(self.stage_history_advance(id, target));
        }
        effects
    }

    fn on_rollback_history_load(&mut self, id: HistorySessionId) -> Vec<Effect> {
        let Some(pending) = self
            .histories
            .get_mut(&id)
            .and_then(|state| state.pending_load.take())
        else {
            return Vec::new();
        };

        let opened: BTreeSet<_> = pending.opened_handle_ids.iter().copied().collect();
        for handle_id in &opened {
            self.history_by_handle.remove(handle_id);
            let _ = self.resolver.unsubscribe(*handle_id);
        }
        let state = self
            .histories
            .get_mut(&id)
            .expect("rollback target remained live while staged handles closed");
        state
            .handles
            .retain(|handle| !opened.contains(&handle.id()));
        state.handle_ids.retain(|handle| !opened.contains(handle));
        state
            .acquisitions
            .retain(|handle, _| !opened.contains(handle));
        if let Some(second) = pending.acquired_tie_second {
            state.acquired_tie_seconds.remove(&second);
        }
        for event_id in pending.added_row_ids {
            if let Some(row) = state.last_rows.remove(&event_id) {
                state
                    .order
                    .remove(&(Reverse(row.event.created_at.as_secs()), event_id));
            }
        }
        state.target_rows = pending.prior_target_rows;
        state.load = pending.prior_load;
        state.last_evidence = pending.prior_evidence;
        state.projection_complete = pending.prior_projection_complete;

        Vec::new()
    }

    fn on_set_active_pubkey(&mut self, pk: Option<PublicKey>) -> Vec<Effect> {
        self.active_pubkey = pk;
        let mut effects = Vec::new();
        // Re-rooting reactive nodes can re-query the store (a `Derived`
        // binding over a reactive field). Degrade to read-only on a
        // persistence failure (issue #122) rather than panic.
        if let Err(e) = self.resolver.set_active_pubkey(pk) {
            self.degrade_store(e, &mut effects);
            return effects;
        }
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        if let Some(pk) = pk {
            // The runtime moves its active signer pointer before delivering
            // this message. Re-arm matching accepted work here as well as
            // on SignerAttached so both ordering cases (activate→attach and
            // attach→activate) converge without polling.
            effects.extend(self.on_signer_attached(pk));
        }
        effects
    }

    // ---- write outbox (D: intent -> signed -> routed -> sent -> acked) --

    /// `Publish` (issues #2/#3 U3): enter durable/at-most-once writes through
    /// `resolver.accept_local` exactly once. The store allocates both ids
    /// and commits the canonical pending row, obligation and receipt before
    /// `Accepted` is observable. Ephemeral uses the distinct receipt-only
    /// door: no pending row and no retry obligation, but still a stable,
    /// reattachable receipt as required by the promoted VISION.
    ///
    /// A `Signed` payload is verified here, at the acceptance boundary,
    /// BEFORE `WriteStatus::Accepted` is ever emitted (#52 Q2). This is the
    /// only publish path in the crate — `Handle::publish` is the sole entry
    /// point regardless of caller (FFI, direct-Rust, `nmp-bdd`'s
    /// `EngineThread`) — so verifying here, rather than at each caller,
    /// makes "a forged `Signed` event can never be published" true
    /// unconditionally instead of entry-point-dependent. A failed verify is
    /// a whole-intent terminal (`WriteStatus::Failed`): no `Accepted`, no
    /// pending write recorded, no `Effect::PublishEvent`.
    ///
    /// Identity resolution (#47): with `identity_override: None` the
    /// single-identity contract holds verbatim — an unsigned draft must be
    /// authored by the CURRENT active account, else fail closed
    /// pre-acceptance. With `Some(pk)` the caller explicitly consents to
    /// publish this one write as `pk`: `pk` must EQUAL the draft's author
    /// (the reducer never restamps a draft; a mismatch fails closed with no
    /// `Accepted`), and when it does the write is accepted with
    /// `signing_pubkey = pk` regardless of the active account — including
    /// while logged out. Acceptance pins `pk` (`expected_pubkey` /
    /// `signing_identity_ref`), so everything downstream — the frozen body,
    /// `RequestSign`, the `SignerAttached` re-arm, restart replay — targets
    /// the override identity forever; a later `set_active_account` cannot
    /// retarget it, and an override with no registered capability parks
    /// durably as `AwaitingCapability` rather than failing or drifting.
    fn on_publish(&mut self, intent: WriteIntent, sink: Box<dyn ReceiptSink>) -> Vec<Effect> {
        let WriteIntent {
            payload,
            durability,
            routing,
            identity_override,
        } = intent;

        let replaceable_base = match &payload {
            WritePayload::UnsignedReplaceableEdit { expected_base, .. } => Some(*expected_base),
            WritePayload::Unsigned(_) | WritePayload::Signed(_) => None,
        };

        let payload_kind = match &payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => unsigned.kind,
            WritePayload::Signed(event) => event.kind,
        };
        if payload_kind == nostr::Kind::Authentication {
            return self.fail_unaccepted(
                sink,
                "kind:22242 is reserved for reducer-owned relay authentication".to_string(),
            );
        }

        if replaceable_base.is_some() && durability == Durability::Ephemeral {
            return self.fail_unaccepted(
                sink,
                "replaceable edits require durable or at-most-once acceptance".to_string(),
            );
        }

        let signing_pubkey = match &payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => match identity_override {
                // #47: explicit per-write consent to publish as `pk`. The
                // override must equal the draft's author — the reducer never
                // restamps a draft to match it — and once it does, the
                // active account is irrelevant (even logged out): acceptance
                // pins `pk` and downstream signing targets it forever.
                Some(pk) if pk == unsigned.pubkey => pk,
                Some(pk) => {
                    return self.fail_unaccepted(
                        sink,
                        format!(
                            "identity override {pk} does not match the unsigned draft author {}",
                            unsigned.pubkey
                        ),
                    );
                }
                // Default single-identity contract, unchanged: the draft's
                // author must be the CURRENT active account, fail closed
                // otherwise.
                None => match self.active_pubkey {
                    Some(active) if active == unsigned.pubkey => active,
                    Some(_) => {
                        return self.fail_unaccepted(
                            sink,
                            "unsigned draft author does not match current active account"
                                .to_string(),
                        );
                    }
                    None => {
                        return self.fail_unaccepted(
                            sink,
                            "unsigned publish requires an active account".to_string(),
                        );
                    }
                },
            },
            // Already-signed payloads are verified verbatim and never ask a
            // local signer, so their author is intrinsically frozen. An
            // explicit override may still name that author (a harmless
            // restatement) — but naming anyone ELSE is a consent/author
            // contradiction and fails closed before acceptance (#47).
            WritePayload::Signed(event) => match identity_override {
                Some(pk) if pk != event.pubkey => {
                    return self.fail_unaccepted(
                        sink,
                        format!(
                            "identity override {pk} does not match the signed event author {}",
                            event.pubkey
                        ),
                    );
                }
                _ => event.pubkey,
            },
        };

        if let WritePayload::Signed(event) = &payload {
            if let Err(err) = event.verify() {
                return self.fail_unaccepted(sink, err.to_string());
            }
        }

        let frozen = match Self::freeze_payload(&payload) {
            Ok(frozen) => frozen,
            Err(reason) => return self.fail_unaccepted(sink, reason),
        };

        let (id, intent_id, already_signed, accepted_signed_event, committed) = if durability
            == Durability::Ephemeral
        {
            match self
                .resolver
                .store_mut()
                .accept_ephemeral(frozen.id, signing_pubkey)
            {
                Ok(receipt_id) => (ReceiptId(receipt_id), None, false, None, None),
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            }
        } else {
            let store_durability = match durability {
                Durability::Durable => WriteDurability::Durable,
                Durability::AtMostOnce => WriteDurability::AtMostOnce,
                Durability::Ephemeral => unreachable!("handled above"),
            };
            let accept = AcceptWrite {
                frozen: frozen.clone(),
                replaceable_base,
                expected_pubkey: signing_pubkey,
                signing_identity_ref: signing_pubkey.to_hex(),
                durability: store_durability,
                routing: Self::routing_snapshot(&routing),
                // Treat an unsigned acceptance as reattachable signer work.
                // If a signer is already present the immediate request below
                // promotes it; if not, restart safely re-requests it.
                sig_state: match payload {
                    WritePayload::Unsigned(_) | WritePayload::UnsignedReplaceableEdit { .. } => {
                        IntentSigState::AwaitingSigner
                    }
                    WritePayload::Signed(_) => IntentSigState::Pending,
                },
                accepted_at: self.clock,
            };
            let LocalAcceptResult { outcome, committed } = match self.resolver.accept_local(accept)
            {
                Ok(value) => value,
                Err(err) => return self.fail_unaccepted(sink, err.to_string()),
            };
            let Some(intent_id) = outcome.journaled_intent_id() else {
                let AcceptOutcome::Refused(reason) = outcome else {
                    unreachable!("only Refused omits journal ids")
                };
                return match reason {
                    nmp_store::RefuseReason::ReplaceableBaseChanged { expected, actual } => self
                        .fail_unaccepted_with_status(
                            sink,
                            WriteStatus::ReplaceableConflict { expected, actual },
                        ),
                    other => self.fail_unaccepted(sink, format!("write refused: {other:?}")),
                };
            };
            let receipt_id = outcome
                .journaled_receipt_id()
                .expect("journaled intent always has a receipt id");
            let accepted_signed_event = match &outcome {
                AcceptOutcome::Duplicate { row, .. } if row.event.sig != sentinel_signature() => {
                    Some(row.event.clone())
                }
                _ => None,
            };
            (
                ReceiptId(receipt_id),
                Some(intent_id),
                accepted_signed_event.is_some(),
                accepted_signed_event,
                Some(committed),
            )
        };

        let mut effects = Vec::new();
        sink.on_status(WriteStatus::Accepted);
        effects.push(Effect::EmitReceipt(id, WriteStatus::Accepted));

        self.pending.insert(
            id,
            PendingWrite {
                durability,
                routing,
                routing_valid: true,
                sinks: vec![Rc::from(sink)],
                intent_id,
                signing_pubkey,
                frozen: frozen.clone(),
                already_signed,
                sign_request_in_flight: false,
                sign_generation: 0,
                event_id: None,
                pending_relays: BTreeSet::new(),
                unstarted_relays: BTreeSet::new(),
                route_blocked_relays: BTreeSet::new(),
                attempt_ordinals: BTreeMap::new(),
                lane_relays: BTreeSet::new(),
            },
        );
        // `intent_id` is `None` only for Ephemeral, which never owns a
        // pending row or a lane -- nothing to index for it (epic #507
        // finding E5).
        if let Some(intent_id) = intent_id {
            self.intent_receipts.insert(intent_id, id);
        }

        if let Some(committed) = committed {
            // A local pending row was committed before Accepted. When it did
            // not alter reactive demand/router shape, expose its exact row
            // facts through the same O(committed delta) projection path as a
            // relay batch. Any demand change keeps the broad refresh oracle.
            self.apply_committed_mutation(committed, &mut effects);
        }

        match payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => {
                if already_signed {
                    self.on_signed(
                        id,
                        accepted_signed_event
                            .expect("already-signed acceptance carries its canonical event"),
                        &mut effects,
                    );
                } else {
                    if let Some(pending) = self.pending.get_mut(&id) {
                        pending.sign_request_in_flight = true;
                        pending.sign_generation += 1;
                        let generation = pending.sign_generation;
                        effects.push(Effect::RequestSign(id, generation, unsigned));
                    }
                }
            }
            WritePayload::Signed(event) => {
                self.on_signed(id, event, &mut effects);
            }
        }
        effects
    }

    /// `SignerCompleted` (plan §3.4 step 2 continuation): the runtime's
    /// signer capability resolved. Explicit rejection and invalid signer
    /// output are whole-intent terminals (`WriteStatus::Failed`). Transport
    /// absence, timeout, and disconnect return the retained obligation to
    /// `AwaitingCapability` so the exact frozen identity can be reattached.
    fn on_signer_completed(
        &mut self,
        id: ReceiptId,
        generation: u64,
        result: Result<SignedEvent, SignerError>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let Some(pending) = self.pending.get_mut(&id) else {
            return effects;
        };
        if !pending.sign_request_in_flight || pending.sign_generation != generation {
            return effects;
        }
        pending.sign_request_in_flight = false;
        match result {
            Ok(event) => self.on_signed(id, event, &mut effects),
            Err(err) => {
                if err.is_terminal() {
                    self.fail_and_compensate(id, err.to_string(), &mut effects);
                } else if let Some(pending) = self.pending.get_mut(&id) {
                    let signing_pubkey = pending.signing_pubkey;
                    Self::notify(pending, WriteStatus::AwaitingCapability);
                    effects.push(Effect::EmitReceipt(id, WriteStatus::AwaitingCapability));
                    effects.push(Effect::RearmSignerIfAvailable(signing_pubkey));
                }
            }
        }
        effects
    }

    fn on_signer_unavailable(&mut self, id: ReceiptId, generation: u64) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some(pending) = self.pending.get_mut(&id) {
            if !pending.sign_request_in_flight || pending.sign_generation != generation {
                return effects;
            }
            pending.sign_request_in_flight = false;
            Self::notify(pending, WriteStatus::AwaitingCapability);
            effects.push(Effect::EmitReceipt(id, WriteStatus::AwaitingCapability));
        }
        effects
    }

    fn on_signer_attached(&mut self, pk: PublicKey) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (id, pending) in &mut self.pending {
            if pending.signing_pubkey == pk
                && pending.event_id.is_none()
                && !pending.already_signed
                && !pending.sign_request_in_flight
            {
                pending.sign_request_in_flight = true;
                pending.sign_generation += 1;
                effects.push(Effect::RequestSign(
                    *id,
                    pending.sign_generation,
                    UnsignedEvent {
                        id: Some(pending.frozen.id),
                        pubkey: pending.frozen.pubkey,
                        created_at: pending.frozen.created_at,
                        kind: pending.frozen.kind,
                        tags: pending.frozen.tags.clone(),
                        content: pending.frozen.content.clone(),
                    },
                ));
            }
        }
        effects
    }

    fn on_cancel_write(&mut self, id: ReceiptId) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.fail_and_compensate(
            id,
            "write cancelled before signing".to_string(),
            &mut effects,
        );
        effects
    }

    /// Shared by the pre-signed (`on_publish`) and signer-completed paths:
    /// `Signed` -> resolve `WriteRouting` -> `Routed` -> `PublishEvent` per
    /// relay -> `Sent` per relay. Route failure (ledger #6) is a whole-
    /// intent `Failed` with NO `PublishEvent` emitted for any relay —
    /// structurally, an unroutable private recipient cannot reach the wire
    /// here because `relays` is never bound in that branch. Every borrow of
    /// `self.pending` below is scoped to its own statement so the map can
    /// be freely read/mutated/removed across steps.
    fn on_signed(&mut self, id: ReceiptId, event: SignedEvent, effects: &mut Vec<Effect>) {
        let Some(pending) = self.pending.get(&id) else {
            return; // unknown/already-resolved receipt id.
        };
        if pending.event_id.is_some() {
            return; // duplicate/delayed signer completion after routing.
        }

        if let Err(reason) = Self::validate_signed_template(&pending.frozen, &event) {
            self.fail_and_compensate(id, reason, effects);
            return;
        }

        let mut co_receipts = Vec::new();
        if let Some(intent_id) = pending.intent_id {
            if !pending.already_signed {
                match self
                    .resolver
                    .store_mut()
                    .promote_signed(intent_id, event.sig)
                {
                    Ok(PromoteOutcome::Promoted { co_signed, .. }) => {
                        // The store atomically promotes every exact-duplicate
                        // co-owner against the same canonical bytes. Advance
                        // each matching in-memory obligation too; otherwise
                        // an offline co-owner could remain stranded forever
                        // behind a row that is already validly signed.
                        for co_intent in co_signed {
                            if let Some((receipt_id, co_pending)) = self
                                .pending
                                .iter_mut()
                                .find(|(_, candidate)| candidate.intent_id == Some(co_intent))
                            {
                                co_pending.already_signed = true;
                                co_receipts.push(*receipt_id);
                            }
                        }
                    }
                    Ok(PromoteOutcome::NotFound) => {
                        self.fail_and_compensate(
                            id,
                            "accepted intent was unavailable for signature promotion".to_string(),
                            effects,
                        );
                        return;
                    }
                    Err(err) => {
                        self.fail_and_compensate(id, err.to_string(), effects);
                        return;
                    }
                }
            }
        }

        for co_receipt in co_receipts {
            self.on_signed(co_receipt, event.clone(), effects);
        }

        if let Some(pending) = self.pending.get_mut(&id) {
            pending.event_id = Some(event.id);
            pending.frozen = event.clone();
        }

        if let Some(pending) = self.pending.get(&id) {
            Self::notify(pending, WriteStatus::Signed(event.id));
            effects.push(Effect::EmitReceipt(id, WriteStatus::Signed(event.id)));
            if !pending.routing_valid {
                return;
            }
        }

        let author_hex = event.pubkey.to_hex();
        let relays = match self
            .pending
            .get(&id)
            .map(|pending| self.resolve_routes(&pending.routing, &author_hex))
        {
            Some(Ok(relays)) => relays,
            Some(Err(reason)) => {
                if let Some(pending) = self.pending.remove(&id) {
                    // No lanes have been bootstrapped for this intent yet at
                    // this point in `on_signed` (that only happens further
                    // below, after routes resolve) -- `lane_relays` is
                    // guaranteed empty, but `intent_receipts` was already
                    // populated at acceptance, so this must still clean it
                    // (epic #507 finding E5).
                    self.forget_pending_indexes(id, &pending);
                    let status = WriteStatus::Failed(reason);
                    Self::notify(&pending, status.clone());
                    effects.push(Effect::EmitReceipt(id, status));
                }
                return;
            }
            None => return,
        };

        self.emit_write_status(id, WriteStatus::Routed(relays.clone()), effects);

        if let Some(write_access) = self
            .pending
            .get(&id)
            .filter(|pending| pending.durability == Durability::Ephemeral)
            .map(|pending| AccessContext::Nip42(pending.signing_pubkey))
        {
            for relay in relays {
                let Ok(correlation) = self.alloc_attempt_correlation() else {
                    continue;
                };
                self.attempt_correlations.insert(
                    correlation,
                    AttemptCorrelationTarget {
                        receipt: id,
                        // The ephemeral handoff rides the intent's
                        // identity-scoped authenticated session (#8 U2),
                        // never the relay's Public read session.
                        session: RelaySessionKey::new(relay.clone(), write_access),
                        lane: None,
                    },
                );
                effects.push(Effect::PublishEvent(
                    RelaySessionKey::new(relay, write_access),
                    event.clone(),
                    correlation,
                ));
            }
            // Ephemeral never owns a durable lane (`intent_id` is `None`),
            // so there is nothing for `forget_pending_indexes` to find, but
            // calling it keeps this a single uniform cleanup discipline for
            // every real `pending` removal (epic #507 finding E5).
            if let Some(pending) = self.pending.remove(&id) {
                self.forget_pending_indexes(id, &pending);
            }
            return;
        }

        let Some((intent_id, write_access)) = self.pending.get(&id).and_then(|pending| {
            pending
                .intent_id
                .map(|intent_id| (intent_id, AccessContext::Nip42(pending.signing_pubkey)))
        }) else {
            return;
        };
        if self
            .resolver
            .store_mut()
            .record_route_revision(intent_id, relays.clone())
            .is_err()
        {
            if let Some(pending) = self.pending.get_mut(&id) {
                pending.route_blocked_relays = relays.clone();
            }
            for relay in relays {
                self.emit_write_status(id, WriteStatus::RoutePersistenceBlocked(relay), effects);
            }
            return;
        }

        let lanes = match self.resolver.store_mut().bootstrap_outbox_lanes(intent_id) {
            Ok(lanes) => lanes,
            Err(_) => {
                // This is the sole call that teaches the reverse index this
                // freshly-signed intent's lanes; a failure here means the
                // index cannot learn whatever lanes may (or may not) exist,
                // so degrade rather than assume "no lanes" (epic #507
                // finding E5).
                self.lane_relay_index_degraded = true;
                for relay in relays {
                    self.emit_write_status(id, WriteStatus::PersistenceBlocked(relay), effects);
                }
                return;
            }
        };
        self.event_to_receipts
            .entry(event.id)
            .or_default()
            .insert(id);
        for lane in lanes {
            let lane_relay = lane.key.relay.clone();
            if let Some(pending) = self.pending.get_mut(&id) {
                if pending.lane_relays.insert(lane_relay.clone()) {
                    self.receipts_by_lane_relay
                        .entry(lane_relay)
                        .or_default()
                        .insert(id);
                }
            }
            if matches!(lane.state, LaneState::WaitingConnection) {
                // The freshly-bootstrapped lane's connectivity check is
                // against the intent's identity-scoped authenticated
                // session (#8 U2), the exact session `schedule_ready` will
                // publish on.
                let session = RelaySessionKey::new(lane.key.relay.clone(), write_access);
                if self.connected_relays.contains(&session) {
                    let _ = self.resolver.store_mut().set_lane_eligible(
                        &lane.key,
                        lane.revision,
                        self.clock,
                    );
                } else {
                    self.emit_write_status(
                        id,
                        WriteStatus::AwaitingRelay {
                            relay: lane.key.relay.clone(),
                        },
                        effects,
                    );
                    effects.push(Effect::EnsureRelay(session));
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }

    fn freeze_payload(payload: &WritePayload) -> Result<SignedEvent, String> {
        match payload {
            WritePayload::Unsigned(unsigned)
            | WritePayload::UnsignedReplaceableEdit { unsigned, .. } => {
                let computed = EventId::new(
                    &unsigned.pubkey,
                    &unsigned.created_at,
                    &unsigned.kind,
                    &unsigned.tags,
                    &unsigned.content,
                );
                if let Some(declared) = unsigned.id {
                    if declared != computed {
                        return Err(
                            "unsigned event carries an id that does not match its body".into()
                        );
                    }
                }
                Ok(SignedEvent::new(
                    computed,
                    unsigned.pubkey,
                    unsigned.created_at,
                    unsigned.kind,
                    unsigned.tags.clone(),
                    unsigned.content.clone(),
                    sentinel_signature(),
                ))
            }
            WritePayload::Signed(event) => Ok(SignedEvent::new(
                event.id,
                event.pubkey,
                event.created_at,
                event.kind,
                event.tags.clone(),
                event.content.clone(),
                sentinel_signature(),
            )),
        }
    }

    fn validate_signed_template(frozen: &SignedEvent, signed: &SignedEvent) -> Result<(), String> {
        if signed.id != frozen.id
            || signed.pubkey != frozen.pubkey
            || signed.created_at != frozen.created_at
            || signed.kind != frozen.kind
            || signed.tags != frozen.tags
            || signed.content != frozen.content
        {
            return Err(
                "signer returned an event that does not match the accepted template".into(),
            );
        }
        signed
            .verify()
            .map_err(|err| format!("signer returned an invalid signature: {err}"))
    }

    fn routing_snapshot(routing: &WriteRouting) -> String {
        match routing {
            WriteRouting::AuthorOutbox => "author-outbox".to_string(),
            WriteRouting::ToInboxes(recipients) => format!(
                "to-inboxes:{}",
                recipients
                    .iter()
                    .map(PublicKey::to_hex)
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PrivateNarrow(route) => format!(
                "private-narrow-hex:{}",
                route
                    .relays
                    .iter()
                    .map(|relay| hex::encode(relay.to_string()))
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            WriteRouting::PinnedHost(auth) => {
                format!("pinned-host-hex:{}", hex::encode(auth.host().to_string()))
            }
        }
    }

    fn parse_routing_snapshot(snapshot: &str) -> Option<WriteRouting> {
        if snapshot == "author-outbox" {
            return Some(WriteRouting::AuthorOutbox);
        }
        if let Some(keys) = snapshot.strip_prefix("to-inboxes:") {
            let recipients = if keys.is_empty() {
                Vec::new()
            } else {
                keys.split(',')
                    .map(PublicKey::from_hex)
                    .collect::<Result<Vec<_>, _>>()
                    .ok()?
            };
            return Some(WriteRouting::ToInboxes(recipients));
        }
        if let Some(encoded) = snapshot.strip_prefix("private-narrow-hex:") {
            let relays = if encoded.is_empty() {
                Vec::new()
            } else {
                encoded
                    .split(',')
                    .map(|part| {
                        let bytes = hex::decode(part).ok()?;
                        let url = String::from_utf8(bytes).ok()?;
                        RelayUrl::parse(&url).ok()
                    })
                    .collect::<Option<Vec<_>>>()?
            };
            return Some(WriteRouting::PrivateNarrow(PrivateRoute {
                relays: NarrowOnly::new(relays),
            }));
        }
        if let Some(encoded) = snapshot.strip_prefix("pinned-host-hex:") {
            let bytes = hex::decode(encoded).ok()?;
            let url = String::from_utf8(bytes).ok()?;
            let host = RelayUrl::parse(&url).ok()?;
            return Some(WriteRouting::PinnedHost(HostAuthority::from_selected_host(
                host,
            )));
        }
        None
    }

    fn fail_unaccepted(&mut self, sink: Box<dyn ReceiptSink>, reason: String) -> Vec<Effect> {
        self.fail_unaccepted_with_status(sink, WriteStatus::Failed(reason))
    }

    fn fail_unaccepted_with_status(
        &mut self,
        sink: Box<dyn ReceiptSink>,
        status: WriteStatus,
    ) -> Vec<Effect> {
        // No store id exists on refusal/persistence failure by contract.
        // This correlation id is stream-local only and never enters the
        // durable receipt namespace.
        let id = match self.alloc_receipt_id() {
            Ok(id) => id,
            Err(err) => return vec![Effect::PublishFailed(err)],
        };
        sink.on_status(status.clone());
        vec![Effect::EmitReceipt(id, status)]
    }

    fn fail_and_compensate(&mut self, id: ReceiptId, reason: String, effects: &mut Vec<Effect>) {
        let Some(pending) = self.pending.remove(&id) else {
            return;
        };

        if let Some(intent_id) = pending.intent_id {
            match self.resolver.store_mut().compensate_write(intent_id) {
                Ok(outcome @ CompensateOutcome::Compensated { .. }) => {
                    // The store compensation already committed; reacting only
                    // re-reads to recompute the graph. A read failure here
                    // (issue #122) degrades to read-only rather than panics.
                    match self
                        .resolver
                        .react_to_compensation(pending.frozen.clone(), &outcome)
                    {
                        Ok(committed) => {
                            self.apply_committed_mutation(committed, effects);
                        }
                        Err(e) => self.degrade_store(e, effects),
                    }
                }
                Ok(CompensateOutcome::NotFound) => {
                    // Promotion already made the row valid. Never retract a
                    // signed row; cancellation/signing errors arriving late
                    // cannot rewrite cache truth.
                    self.pending.insert(id, pending);
                    return;
                }
                Err(err) => {
                    // Compensation itself failed atomically. Keep the
                    // in-memory obligation so the caller can retry rather
                    // than losing ownership of a still-visible pending row.
                    // Crucially, do NOT emit terminal Failed: persistence
                    // did not commit the terminal transition, so claiming it
                    // did would contradict both the row and journal. U4 owns
                    // durable retry scheduling; a later explicit cancel or
                    // signer completion can re-enter this door.
                    self.pending.insert(id, pending);
                    let _persistence_error = err;
                    return;
                }
            }
        }

        // Reached only when `intent_id` was `None` (Ephemeral -- nothing to
        // clean) or compensation actually committed (a real, permanent
        // removal): both `NotFound`/`Err` arms above reinsert `pending`
        // untouched and return early, so the indexes must stay untouched
        // for those (epic #507 finding E5).
        self.forget_pending_indexes(id, &pending);
        Self::notify(&pending, WriteStatus::Failed(reason.clone()));
        effects.push(Effect::EmitReceipt(id, WriteStatus::Failed(reason)));
    }

    /// Resolve a `WriteRouting` to a concrete relay set using the SAME
    /// `RelayDirectory` lane facts the read path routes against (plan
    /// §3.4). `AuthorOutbox` reuses the author's NIP-65 write-relay lane
    /// directly (the same fact `nmp_router::route::build_candidates` reads
    /// for outbox coverage-solving, minus the 2-relay-min solver — a write
    /// fans out to every known write relay, it does not need coverage-
    /// solving). `PrivateNarrow` never consults the directory at all — its
    /// relay set is exactly whatever the caller pre-narrowed into the
    /// `NarrowOnly` set, empty or not (ledger #6's fail-closed mechanism).
    ///
    /// `ToInboxes` fans a p-tagged inbox write out to each recipient's
    /// NIP-65 READ-marked relays (`RelayDirectory::read_relays`, lane
    /// `Nip65Read`) — the read side of the SAME kind:10002 winner the read
    /// path consults for authors' write relays (`routing-and-ownership.md`
    /// §2.4). It NEVER consults a recipient's `write_relays`/`extra_relays`:
    /// addressing inbox traffic to a recipient's write relays under-delivers
    /// and leaks metadata (issue #19). A recipient whose read/inbox relays
    /// are unknown — never seen a kind:10002, or one that declares only
    /// write-marked relays — fails the whole intent CLOSED with a typed
    /// `Failed` before any `PublishEvent`, rather than guessing a relay;
    /// recipient discovery rides the existing kind:10002 `sync_discovery`
    /// machinery, so a later winner simply makes the retry routable.
    ///
    /// `PinnedHost` (#115) also never consults the directory — like
    /// `PrivateNarrow`, its one relay is exactly whatever the caller
    /// asserted via `HostAuthority::from_selected_host`. Unlike
    /// `PrivateNarrow`, an empty/unroutable state is structurally
    /// unreachable (`HostAuthority` always carries exactly one well-formed
    /// `RelayUrl`), so this arm is infallible where `PrivateNarrow`'s is
    /// not.
    fn resolve_routes(
        &self,
        routing: &WriteRouting,
        author_hex: &str,
    ) -> Result<BTreeSet<RelayUrl>, String> {
        match routing {
            WriteRouting::AuthorOutbox => {
                let author = author_hex.to_string();
                let relays: BTreeSet<RelayUrl> = self
                    .directory
                    .write_relays(&author)
                    .into_iter()
                    .map(|lr| lr.url)
                    .collect();
                if relays.is_empty() {
                    Err(format!("no write relays known for author {author_hex}"))
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::ToInboxes(recipients) => {
                let mut relays = BTreeSet::new();
                for pk in recipients {
                    let hex = pk.to_hex();
                    // Read/inbox relays ONLY (lane `Nip65Read`) — never a
                    // recipient's write/extra relays. Fail CLOSED per
                    // recipient: an unknown or write-only recipient has no
                    // inbox relay, and guessing one would leak/under-deliver.
                    let inbox: Vec<RelayUrl> = self
                        .directory
                        .read_relays(&hex)
                        .into_iter()
                        .map(|lr| lr.url)
                        .collect();
                    if inbox.is_empty() {
                        return Err(format!(
                            "no NIP-65 read/inbox relays known for recipient {hex} -- \
                             inbox route fails closed, never falls back to write relays"
                        ));
                    }
                    relays.extend(inbox);
                }
                if relays.is_empty() {
                    Err("ToInboxes routing has no recipients".to_string())
                } else {
                    Ok(relays)
                }
            }
            WriteRouting::PrivateNarrow(route) => {
                if route.relays.is_empty() {
                    Err(
                        "private route has no narrow relay set -- fails closed, never widens to a public relay"
                            .to_string(),
                    )
                } else {
                    Ok(route.relays.iter().cloned().collect())
                }
            }
            WriteRouting::PinnedHost(auth) => Ok(BTreeSet::from([auth.host()])),
        }
    }

    /// An `OK` frame resolves exactly one (event, relay) pair's pending
    /// ack. An `OK` for an event/relay this reducer isn't tracking (unknown
    /// event id, already-terminal receipt, duplicate OK, or an `Ephemeral`
    /// write that was already forgotten) is silently ignored — it is an
    /// untrusted-network fact, not a caller error.
    fn handle_write_ack(
        &mut self,
        event_id: EventId,
        status: bool,
        message: String,
        session: &RelaySessionKey,
        effects: &mut Vec<Effect>,
    ) {
        let Some(ids) = self.event_to_receipts.get(&event_id).cloned() else {
            return;
        };
        let class = classify_relay_ack(status, &message);
        for id in ids {
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let Some(intent_id) = pending.intent_id else {
                continue;
            };
            // An OK is only trusted from the exact session this pending write
            // publishes on (#8 U2: the intent's identity-scoped Nip42 write
            // session, frozen at acceptance). An ack arriving on any other
            // context's session for the same URL — including the Public read
            // session — must never advance this write lane.
            let expected_session = RelaySessionKey::new(
                session.relay.clone(),
                AccessContext::Nip42(pending.signing_pubkey),
            );
            if &expected_session != session {
                continue;
            }
            let relay = &session.relay;
            let key = LaneKey {
                intent_id,
                relay: relay.clone(),
            };
            let lane = self
                .resolver
                .store()
                .recover_outbox_lanes(intent_id)
                .ok()
                .and_then(|lanes| lanes.into_iter().find(|lane| lane.key == key));
            let Some(lane) = lane else {
                continue;
            };
            let LaneState::InFlight {
                ordinal,
                phase: InFlightPhase::AwaitingAck { .. },
            } = lane.state
            else {
                continue;
            };

            match &class {
                RelayAckClass::Acked => {
                    if self
                        .resolver
                        .store_mut()
                        .finish_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            AttemptOutcome::Acked,
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(id, WriteStatus::Acked(relay.clone()), effects);
                        self.close_if_all_lanes_terminal(id);
                    }
                }
                RelayAckClass::Rejected => {
                    if self
                        .resolver
                        .store_mut()
                        .finish_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            AttemptOutcome::Rejected(message.clone()),
                            self.clock,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::Rejected(relay.clone(), message.clone()),
                            effects,
                        );
                        self.close_if_all_lanes_terminal(id);
                    }
                }
                RelayAckClass::Transient(cause) => {
                    let eligible_at = self.clock + retry_delay_secs(&key, ordinal);
                    if self
                        .resolver
                        .store_mut()
                        .set_lane_transient(
                            &key,
                            lane.revision,
                            ordinal,
                            eligible_at,
                            *cause,
                            Some(message.clone()),
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::RetryEligible {
                                relay: relay.clone(),
                                attempt: ordinal,
                                eligible_at,
                            },
                            effects,
                        );
                    }
                }
                RelayAckClass::WaitingAuth => {
                    self.auth_probe_sessions.remove(session);
                    self.auth_required_sessions.insert(session.clone());
                    if self
                        .resolver
                        .store_mut()
                        .suspend_lane_attempt(
                            &key,
                            lane.revision,
                            ordinal,
                            self.clock,
                            TransientCause::AuthRequired,
                            Some(message.clone()),
                            true,
                        )
                        .is_ok()
                    {
                        self.remove_active_lane(id, relay);
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingAuth {
                                relay: relay.clone(),
                            },
                            effects,
                        );
                    }
                }
            }
        }
        effects.extend(self.schedule_ready(self.clock));
    }
    fn suspend_disconnected_lanes(&mut self, session: &RelaySessionKey, effects: &mut Vec<Effect>) {
        let Ok(lanes) = self.recover_all_lanes() else {
            self.retry_scheduler_blocked = true;
            return;
        };
        for (id, lane) in lanes {
            // Only lanes riding EXACTLY this session suspend (#8): a different
            // access context's session for the same URL did not drop. Since
            // the AUTH-reducer wave (#8 U2) write lanes ride the intent's
            // identity-scoped Nip42 session; a lane whose receipt has no
            // live pending entry is skipped.
            let Some(signing_pubkey) = self.pending.get(&id).map(|pending| pending.signing_pubkey)
            else {
                continue;
            };
            if RelaySessionKey::new(lane.key.relay.clone(), AccessContext::Nip42(signing_pubkey))
                != *session
            {
                continue;
            }
            let relay = &session.relay;
            match lane.state {
                LaneState::Eligible { .. } => {
                    if self
                        .resolver
                        .store_mut()
                        .set_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingRelay {
                                relay: relay.clone(),
                            },
                            effects,
                        );
                    }
                }
                LaneState::InFlight {
                    ordinal,
                    phase: InFlightPhase::AwaitingAck { .. },
                } => {
                    let durability = self.pending.get(&id).map(|pending| pending.durability);
                    if durability == Some(Durability::AtMostOnce) {
                        if self
                            .resolver
                            .store_mut()
                            .finish_lane_attempt(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                AttemptOutcome::OutcomeUnknown,
                                self.clock,
                            )
                            .is_ok()
                        {
                            self.remove_active_lane(id, relay);
                            self.emit_write_status(
                                id,
                                WriteStatus::OutcomeUnknown(relay.clone()),
                                effects,
                            );
                            self.close_if_all_lanes_terminal(id);
                        }
                    } else {
                        let eligible_at = self.clock + retry_delay_secs(&lane.key, ordinal);
                        if self
                            .resolver
                            .store_mut()
                            .set_lane_transient(
                                &lane.key,
                                lane.revision,
                                ordinal,
                                eligible_at,
                                TransientCause::ConnectionLost,
                                Some("connection lost while awaiting ACK".to_string()),
                            )
                            .is_ok()
                        {
                            self.remove_active_lane(id, relay);
                            self.emit_write_status(
                                id,
                                WriteStatus::RetryEligible {
                                    relay: relay.clone(),
                                    attempt: ordinal,
                                    eligible_at,
                                },
                                effects,
                            );
                        }
                    }
                }
                LaneState::WaitingAuth => {
                    // A `WaitingAuth` park is authenticated-generation-scoped:
                    // the relay demanded auth on THIS socket, and that grant
                    // (and any in-flight challenge) died with the disconnect.
                    // Fall the lane back to `WaitingConnection` so the ordinary
                    // reconnect wake (`wake_relay_lanes(.., auth_only=false)`)
                    // re-drives it — a fresh generation re-sends the event,
                    // re-provokes the challenge, re-parks, authenticates, and
                    // finally wakes via `finish_auth_ok`. Leaving it
                    // `WaitingAuth` here would strand it: the ONLY `WaitingAuth`
                    // wake is `finish_auth_ok`, which for a lazy-challenging
                    // relay never fires again without a client-provoked EVENT.
                    if self
                        .resolver
                        .store_mut()
                        .set_lane_waiting(&lane.key, lane.revision, false)
                        .is_ok()
                    {
                        self.emit_write_status(
                            id,
                            WriteStatus::AwaitingRelay {
                                relay: relay.clone(),
                            },
                            effects,
                        );
                    }
                }
                LaneState::WaitingConnection
                | LaneState::Transient { .. }
                | LaneState::InFlight {
                    phase: InFlightPhase::AwaitingHandoff,
                    ..
                }
                | LaneState::LegacyInFlight { .. }
                | LaneState::Terminal { .. } => {}
            }
        }
    }
    fn alloc_receipt_id(&mut self) -> Result<ReceiptId, PublishError> {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let current = self
            .next_unaccepted_receipt
            .ok_or(PublishError::ReceiptCorrelationIdExhausted)?;
        debug_assert!(current >= FIRST_UNACCEPTED_ID);
        self.next_unaccepted_receipt = (current > FIRST_UNACCEPTED_ID).then_some(current - 1);
        Ok(ReceiptId(current))
    }

    #[cfg(test)]
    fn set_next_unaccepted_receipt_for_test(&mut self, next: Option<u64>) {
        assert!(next.is_none_or(|id| id >= (1u64 << 63)));
        self.next_unaccepted_receipt = next;
    }

    fn notify(pending: &PendingWrite, status: WriteStatus) {
        for sink in &pending.sinks {
            sink.on_status(status.clone());
        }
    }

    // ---- transport wiring (slot bookkeeping only — C owns the pool) -----

    /// `u64::MAX` is structurally reserved for [`AUTH_SEQUENCE_SENTINEL`]:
    /// the counter treats it as already-exhausted and never issues it, so a
    /// REAL epoch/operation sequence can never compare equal to the
    /// counter-exhausted fallback epoch `on_auth_challenge`/
    /// `on_auth_restricted` install. Sentinel distinctness therefore no
    /// longer rests on the `Error`-phase guard alone (#8 U2's deferred
    /// latent item): even a registry or correlation path that only compares
    /// epochs is safe.
    fn mint_auth_sequence(next: &mut Option<u64>) -> Option<u64> {
        let issued = (*next)?;
        if issued == AUTH_SEQUENCE_SENTINEL {
            *next = None;
            return None;
        }
        *next = issued.checked_add(1);
        Some(issued)
    }

    fn mint_auth_epoch(
        &mut self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> Option<AuthEpoch> {
        Some(AuthEpoch {
            handle,
            session: session.clone(),
            sequence: Self::mint_auth_sequence(&mut self.next_auth_epoch)?,
        })
    }

    fn mint_auth_operation(&mut self, epoch: &AuthEpoch) -> Option<AuthOpToken> {
        Some(AuthOpToken {
            epoch: epoch.clone(),
            sequence: Self::mint_auth_sequence(&mut self.next_auth_operation)?,
        })
    }

    fn exact_current_auth_epoch(&self, epoch: &AuthEpoch) -> bool {
        self.connected_relays.contains(&epoch.session)
            && matches!(
                self.slot_to_relay.get(&epoch.handle.slot),
                Some((handle, session)) if *handle == epoch.handle && *session == epoch.session
            )
            && self
                .auth_sessions
                .get(&epoch.session)
                .is_some_and(|state| state.epoch == *epoch)
    }

    pub(crate) fn is_current_transport_session(
        &self,
        handle: TransportRelayHandle,
        session: &RelaySessionKey,
    ) -> bool {
        self.connected_relays.contains(session)
            && matches!(
                self.slot_to_relay.get(&handle.slot),
                Some((current, current_session))
                    if *current == handle && current_session == session
            )
    }

    fn close_protected_reqs(&self, session: &RelaySessionKey) -> Option<Effect> {
        let ops: Vec<_> = self
            .router
            .plan()
            .reqs
            .get(session)?
            .iter()
            .map(|req| WireOp::Close(req.sub_id.clone()))
            .collect();
        (!ops.is_empty()).then(|| {
            Effect::Wire(WireDelta {
                ops: vec![(session.clone(), ops)],
            })
        })
    }

    fn park_relay_lanes_for_auth(&mut self, session: &RelaySessionKey, effects: &mut Vec<Effect>) {
        let Ok(lanes) = self.recover_all_lanes() else {
            self.retry_scheduler_blocked = true;
            return;
        };
        for (id, lane) in lanes {
            let Some(pending) = self.pending.get(&id) else {
                continue;
            };
            let lane_session = RelaySessionKey::new(
                lane.key.relay.clone(),
                AccessContext::Nip42(pending.signing_pubkey),
            );
            if &lane_session != session
                || !matches!(
                    lane.state,
                    LaneState::Eligible { .. } | LaneState::WaitingConnection
                )
            {
                continue;
            }
            if self
                .resolver
                .store_mut()
                .set_lane_waiting(&lane.key, lane.revision, true)
                .is_err()
            {
                self.retry_scheduler_blocked = true;
                continue;
            }
            self.emit_write_status(
                id,
                WriteStatus::AwaitingAuth {
                    relay: session.relay.clone(),
                },
                effects,
            );
        }
    }

    fn invalidate_auth_epoch(
        &mut self,
        session: &RelaySessionKey,
        close_wire: bool,
        effects: &mut Vec<Effect>,
    ) -> Option<AuthSessionState> {
        let was_ready = self.auth_ready_sessions.remove(session).is_some();
        self.attribution.clear_session(session);
        if close_wire && was_ready {
            if let Some(close) = self.close_protected_reqs(session) {
                effects.push(close);
            }
        }
        let previous = self.auth_sessions.remove(session);
        if let Some(state) = previous.as_ref() {
            effects.push(Effect::RelayAuth(AuthEffect::Cancel(state.epoch.clone())));
        }
        // Park only when the relay actually REQUIRED auth for this session
        // (`auth_required_sessions`: challenge, auth-required write ack, or
        // restricted close — every path that creates reducer-known AUTH
        // truth inserts it first). A session that was never required has
        // nothing to invalidate, and its writes deliberately proceed on the
        // ordinary connectivity path (`schedule_ready`'s gate mirrors this):
        // parking such lanes as `WaitingAuth` would wedge every write to a
        // relay that never challenges, because the ONLY wake for
        // `WaitingAuth` is `finish_auth_ok` — which for that relay never
        // fires.
        if self.auth_required_sessions.contains(session) {
            self.park_relay_lanes_for_auth(session, effects);
        }
        previous
    }

    fn on_auth_challenge(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
        challenge: String,
    ) -> Vec<Effect> {
        let AccessContext::Nip42(expected_pubkey) = session.access else {
            return Vec::new();
        };
        self.auth_probe_sessions.remove(&session);
        self.auth_required_sessions.insert(session.clone());
        let mut effects = Vec::new();
        let previous = self.invalidate_auth_epoch(&session, true, &mut effects);
        let last_created_at = previous.as_ref().and_then(|state| state.last_created_at);
        let fallback_epoch = previous.map(|state| state.epoch);
        let Some(epoch) = self.mint_auth_epoch(handle, &session) else {
            self.auth_sessions.insert(
                session.clone(),
                AuthSessionState {
                    epoch: fallback_epoch.unwrap_or(AuthEpoch {
                        handle,
                        session,
                        sequence: AUTH_SEQUENCE_SENTINEL,
                    }),
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_handles(&mut effects);
            return effects;
        };
        if challenge.is_empty() {
            self.auth_sessions.insert(
                session,
                AuthSessionState {
                    epoch,
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_handles(&mut effects);
            return effects;
        }
        let Some(token) = self.mint_auth_operation(&epoch) else {
            self.auth_sessions.insert(
                session,
                AuthSessionState {
                    epoch,
                    challenge,
                    last_created_at,
                    policy_instance: None,
                    signer_instance: None,
                    phase: AuthSessionPhase::Error,
                },
            );
            self.refresh_all_handles(&mut effects);
            return effects;
        };
        self.auth_sessions.insert(
            session,
            AuthSessionState {
                epoch: epoch.clone(),
                challenge: challenge.clone(),
                last_created_at,
                policy_instance: None,
                signer_instance: None,
                phase: AuthSessionPhase::AwaitingPolicy {
                    token: token.clone(),
                },
            },
        );
        effects.push(Effect::RelayAuth(AuthEffect::RequestPolicy {
            token,
            expected_pubkey,
            challenge,
        }));
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_auth_restricted(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if session.access == AccessContext::Public {
            return Vec::new();
        }
        self.auth_probe_sessions.remove(&session);
        self.auth_required_sessions.insert(session.clone());
        let mut effects = Vec::new();
        let previous = self.invalidate_auth_epoch(&session, true, &mut effects);
        let last_created_at = previous.as_ref().and_then(|state| state.last_created_at);
        let fallback_epoch = previous.map(|state| state.epoch);
        let epoch = self
            .mint_auth_epoch(handle, &session)
            .or(fallback_epoch)
            .unwrap_or(AuthEpoch {
                handle,
                session: session.clone(),
                sequence: AUTH_SEQUENCE_SENTINEL,
            });
        self.auth_sessions.insert(
            session,
            AuthSessionState {
                epoch,
                challenge: String::new(),
                last_created_at,
                policy_instance: None,
                signer_instance: None,
                phase: AuthSessionPhase::Denied,
            },
        );
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_auth_policy_completed(
        &mut self,
        token: AuthOpToken,
        instance: Option<AuthCapabilityInstance>,
        outcome: AuthPolicyOutcome,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let session = token.epoch.session.clone();
        let Some(mut state) = self.auth_sessions.remove(&session) else {
            return Vec::new();
        };
        if !matches!(
            &state.phase,
            AuthSessionPhase::AwaitingPolicy { token: current } if *current == token
        ) {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let missing_capability = instance.is_none()
            && state.policy_instance.is_none()
            && matches!(outcome, AuthPolicyOutcome::Unavailable);
        let exact_bound = instance.is_some() && instance == state.policy_instance;
        if !missing_capability && !exact_bound {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let mut effects = Vec::new();
        match outcome {
            AuthPolicyOutcome::Allow => {
                let AccessContext::Nip42(expected_pubkey) = state.epoch.session.access else {
                    return Vec::new();
                };
                let clock = self.clock.as_secs();
                let minimum = match state.last_created_at {
                    Some(last) => {
                        let Some(next) = last.as_secs().checked_add(1) else {
                            state.phase = AuthSessionPhase::Error;
                            self.auth_sessions.insert(session, state);
                            self.refresh_all_handles(&mut effects);
                            return effects;
                        };
                        next.max(clock)
                    }
                    None => clock,
                };
                let Some(maximum) = clock.checked_add(AUTH_MAX_FUTURE_SECS) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_handles(&mut effects);
                    return effects;
                };
                if minimum > maximum {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_handles(&mut effects);
                    return effects;
                }
                let created_at = Timestamp::from(minimum);
                let unsigned =
                    EventBuilder::auth(state.challenge.clone(), state.epoch.session.relay.clone())
                        .custom_created_at(created_at)
                        .build(expected_pubkey);
                let Some(sign_token) = self.mint_auth_operation(&state.epoch) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_handles(&mut effects);
                    return effects;
                };
                state.last_created_at = Some(created_at);
                state.policy_instance = instance;
                state.phase = AuthSessionPhase::AwaitingSignature {
                    token: sign_token.clone(),
                    unsigned: unsigned.clone(),
                };
                effects.push(Effect::RelayAuth(AuthEffect::RequestSignature {
                    token: sign_token,
                    unsigned: Box::new(unsigned),
                }));
            }
            AuthPolicyOutcome::Deny { reason: _ } => state.phase = AuthSessionPhase::Denied,
            AuthPolicyOutcome::Unavailable | AuthPolicyOutcome::Error { reason: _ } => {
                state.phase = AuthSessionPhase::Error;
            }
        }
        self.auth_sessions.insert(session, state);
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn signed_auth_matches_frozen(unsigned: &UnsignedEvent, signed: &SignedEvent) -> bool {
        unsigned.id == Some(signed.id)
            && unsigned.pubkey == signed.pubkey
            && unsigned.created_at == signed.created_at
            && unsigned.kind == signed.kind
            && unsigned.tags == signed.tags
            && unsigned.content == signed.content
            && signed.verify().is_ok()
    }

    fn auth_source_status(state: &AuthSessionState) -> SourceStatus {
        match &state.phase {
            AuthSessionPhase::AwaitingPolicy { .. } => SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingPolicy,
            },
            AuthSessionPhase::AwaitingSignature { .. } => SourceStatus::AwaitingAuth {
                phase: AuthPhase::AwaitingSignature,
            },
            AuthSessionPhase::AwaitingSend { .. } | AuthSessionPhase::AwaitingOk { .. } => {
                SourceStatus::AwaitingAuth {
                    phase: AuthPhase::AwaitingRelayAck,
                }
            }
            AuthSessionPhase::Ready { .. } => SourceStatus::Requesting,
            AuthSessionPhase::Denied => SourceStatus::AuthDenied,
            AuthSessionPhase::Error => SourceStatus::Error,
        }
    }

    /// The reducer's current per-session AUTH truth, projected into the
    /// evidence vocabulary for `acquisition_evidence` (#8 U2). Sessions
    /// without an entry are the "connected but never challenged" case the
    /// evidence layer defaults to `AwaitingAuth { AwaitingChallenge }`.
    fn auth_status_map(&self) -> BTreeMap<RelaySessionKey, SourceStatus> {
        self.auth_sessions
            .iter()
            .map(|(session, state)| (session.clone(), Self::auth_source_status(state)))
            .collect()
    }

    fn on_auth_signer_completed(
        &mut self,
        token: AuthOpToken,
        instance: Option<AuthCapabilityInstance>,
        outcome: AuthSignerOutcome,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let session = token.epoch.session.clone();
        let Some(mut state) = self.auth_sessions.remove(&session) else {
            return Vec::new();
        };
        let unsigned = match &state.phase {
            AuthSessionPhase::AwaitingSignature {
                token: current,
                unsigned,
            } if *current == token => unsigned.clone(),
            _ => {
                self.auth_sessions.insert(session, state);
                return Vec::new();
            }
        };
        let missing_capability = instance.is_none()
            && state.signer_instance.is_none()
            && matches!(outcome, AuthSignerOutcome::Unavailable);
        let exact_bound = instance.is_some() && instance == state.signer_instance;
        if !missing_capability && !exact_bound {
            self.auth_sessions.insert(session, state);
            return Vec::new();
        }
        let mut effects = Vec::new();
        match outcome {
            AuthSignerOutcome::Signed(event)
                if Self::signed_auth_matches_frozen(&unsigned, &event) =>
            {
                let Some(send_token) = self.mint_auth_operation(&state.epoch) else {
                    state.phase = AuthSessionPhase::Error;
                    self.auth_sessions.insert(session, state);
                    self.refresh_all_handles(&mut effects);
                    return effects;
                };
                state.phase = AuthSessionPhase::AwaitingSend {
                    token: send_token.clone(),
                    event_id: event.id,
                    early_ok: None,
                };
                effects.push(Effect::RelayAuth(AuthEffect::Send {
                    token: send_token,
                    epoch: state.epoch.clone(),
                    event: Box::new(event),
                }));
            }
            AuthSignerOutcome::Rejected { reason: _ } => state.phase = AuthSessionPhase::Denied,
            AuthSignerOutcome::Signed(_)
            | AuthSignerOutcome::Unavailable
            | AuthSignerOutcome::Error { .. } => {
                state.phase = AuthSessionPhase::Error;
            }
        }
        self.auth_sessions.insert(session, state);
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_auth_capability_bound(
        &mut self,
        token: AuthOpToken,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let Some(state) = self.auth_sessions.get_mut(&token.epoch.session) else {
            return Vec::new();
        };
        match (&state.phase, capability) {
            (AuthSessionPhase::AwaitingPolicy { token: current }, AuthCapability::Policy)
                if *current == token && state.policy_instance.is_none() =>
            {
                state.policy_instance = Some(instance);
            }
            (
                AuthSessionPhase::AwaitingSignature { token: current, .. },
                AuthCapability::Signer,
            ) if *current == token && state.signer_instance.is_none() => {
                state.signer_instance = Some(instance);
            }
            _ => return Vec::new(),
        }
        Vec::new()
    }

    fn on_auth_send_completed(
        &mut self,
        token: AuthOpToken,
        outcome: AuthSendOutcome,
    ) -> Vec<Effect> {
        if !self.exact_current_auth_epoch(&token.epoch) {
            return Vec::new();
        }
        let session = token.epoch.session.clone();
        let Some(mut state) = self.auth_sessions.remove(&session) else {
            return Vec::new();
        };
        let (event_id, early_ok) = match &state.phase {
            AuthSessionPhase::AwaitingSend {
                token: current,
                event_id,
                early_ok,
            } if *current == token => (*event_id, *early_ok),
            _ => {
                self.auth_sessions.insert(session, state);
                return Vec::new();
            }
        };
        let mut effects = Vec::new();
        match outcome {
            AuthSendOutcome::Accepted => {
                if let Some(status) = early_ok {
                    return self.finish_auth_ok(&session, state, event_id, status);
                }
                state.phase = AuthSessionPhase::AwaitingOk { event_id };
            }
            AuthSendOutcome::Unavailable => state.phase = AuthSessionPhase::Error,
        }
        self.auth_sessions.insert(session, state);
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_auth_capability_invalidated(
        &mut self,
        pubkey: PublicKey,
        capability: AuthCapability,
        instance: AuthCapabilityInstance,
    ) -> Vec<Effect> {
        let sessions: Vec<_> = self
            .auth_sessions
            .iter()
            .filter_map(|(session, state)| {
                let owns_instance = match capability {
                    AuthCapability::Policy => state.policy_instance == Some(instance),
                    AuthCapability::Signer => state.signer_instance == Some(instance),
                };
                (session.access == AccessContext::Nip42(pubkey) && owns_instance)
                    .then(|| session.clone())
            })
            .collect();
        let mut effects = Vec::new();
        for session in sessions {
            if let Some(mut state) = self.invalidate_auth_epoch(&session, true, &mut effects) {
                state.phase = AuthSessionPhase::Error;
                self.auth_sessions.insert(session, state);
            }
        }
        self.refresh_all_handles(&mut effects);
        effects
    }

    fn on_relay_connected(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if self
            .slot_to_relay
            .get(&handle.slot)
            .is_some_and(|(current, _)| current.generation > handle.generation)
        {
            return Vec::new();
        }
        let mut effects = Vec::new();
        let same_physical_session = matches!(
            self.slot_to_relay.get(&handle.slot),
            Some((current, current_session)) if *current == handle && *current_session == session
        );
        let open_failure_cleared = self.relay_open_failures.remove(&session).is_some();
        if let Some((_, displaced_session)) = self.slot_to_relay.get(&handle.slot).cloned() {
            if displaced_session != session {
                // A pool slot has one physical owner. If a newer connection
                // replaces its access context before an old disconnect arrives,
                // release the displaced session here; otherwise its AUTH epoch
                // and apparent connectivity could survive forever even though
                // no transport handle can ever make them current again.
                self.invalidate_auth_epoch(&displaced_session, false, &mut effects);
                self.connected_relays.remove(&displaced_session);
                self.auth_probe_sessions.remove(&displaced_session);
            }
        }
        // A fresh connection generation is NEVER pre-authorized (#8): any
        // AUTH readiness earned by an earlier generation of this session
        // died with that socket. Only the AUTH reducer's own ready
        // transition (`finish_auth_ok`, on the exact-generation OK) re-arms
        // it once this generation's handshake completes.
        self.invalidate_auth_epoch(&session, false, &mut effects);
        self.slot_to_relay
            .insert(handle.slot, (handle, session.clone()));
        // A connection can also exist solely for a compiled/persisted write
        // route. It is live for the durable write scheduler and ACK
        // attribution, but it must never receive read replay/probing unless
        // the CURRENT read plan admits that exact SESSION.
        let planned_read_reqs = self.router.plan().reqs.get(&session).cloned();
        // Feeds `AcquisitionEvidence.sources[_].status` (`evidence.rs`):
        // this session is now `Requesting` (or, protected, `AwaitingAuth`),
        // never again `Connecting` for the lifetime of this `EngineCore`
        // (`ever_connected_relays` is append-only -- a later drop reads
        // `Disconnected`, not `Connecting`, per the doc's "was connected,
        // then dropped" fact).
        self.connected_relays.insert(session.clone());
        self.ever_connected_relays.insert(session.clone());
        if !same_physical_session && session.access != AccessContext::Public {
            if self.auth_required_sessions.contains(&session) {
                self.auth_probe_sessions.remove(&session);
            } else {
                self.auth_probe_sessions.insert(session.clone(), handle);
            }
        }
        // Reconnect (new generation): clear stale attribution, then replay
        // + re-snapshot every currently-planned REQ for this session (ruling
        // §2: "a replayed sub on the new generation gets fresh snapshots").
        self.attribution.clear_session(&session);
        // ONLY a Public session replays its planned REQs at connect time. A
        // protected session's REQs park until the AUTH reducer's ready
        // transition (`finish_auth_ok`) proves THIS generation completed
        // AUTH (#8) — sending them earlier would leak the protected demand
        // onto an unauthenticated socket and record attribution snapshots no
        // honest EOSE can ever discharge.
        if session.access == AccessContext::Public {
            if let Some(reqs) = planned_read_reqs.as_ref() {
                for req in reqs {
                    self.attribution.record_send(
                        &session,
                        &req.sub_id,
                        &req.filter,
                        req.absorbed.clone(),
                    );
                }
                if !reqs.is_empty() {
                    effects.push(Effect::Replay(session.clone(), reqs.clone()));
                }
            }
        }
        // NIP-11 is one-shot HTTP evidence, not a stream. Resolve it off the
        // reducer thread before deciding whether a behavioral NIP-77 probe
        // is useful. Explicit negative advertisement can avoid known-noisy
        // probes; positive advertisement can NEVER mint `ProbedRelay`.
        // A connection outside the current read plan has no authority to
        // create either acquisition or capability-probe work.
        if planned_read_reqs.is_some() {
            effects.push(Effect::FetchRelayInformation(session.relay.clone()));
        }
        // A relay coming online can flip a handle's `AcquisitionEvidence`
        // (`Connecting` -> `Requesting`) with no coverage/row change at all
        // -- refresh so that becomes observable via `EmitRows`, same as an
        // EOSE-driven watermark advance below.
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects.extend(self.wake_relay_lanes(&session, false));
        if open_failure_cleared {
            effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        }
        effects
    }

    fn on_auth_probe_released(
        &mut self,
        handle: TransportRelayHandle,
        session: RelaySessionKey,
    ) -> Vec<Effect> {
        if !self
            .slot_to_relay
            .get(&handle.slot)
            .is_some_and(|(current, current_session)| {
                *current == handle && *current_session == session
            })
        {
            return Vec::new();
        }
        if self.auth_ready_sessions.get(&session) == Some(&handle) {
            return vec![Effect::ReleaseInitialRead(handle)];
        }
        if self.auth_probe_sessions.get(&session) != Some(&handle) {
            return Vec::new();
        }
        self.auth_probe_sessions.remove(&session);
        let mut effects = vec![Effect::ReleaseInitialRead(handle)];
        effects.extend(self.wake_relay_lanes(&session, true));
        effects
    }

    fn on_relay_information_resolved(
        &mut self,
        url: RelayUrl,
        information: Option<RelayInformationCapabilityEvidence>,
    ) -> Vec<Effect> {
        let advertises_nip77 = information
            .as_ref()
            .and_then(|information| information.supported_nips.as_ref())
            .map(|nips| nips.contains(&77));
        // NIP-11/NIP-77 capability evidence belongs to the PUBLIC session
        // only (#8): the document is unauthenticated HTTP and the probe runs
        // over the unauthenticated socket, so a URL planned solely under
        // protected sessions retains no document and probes nothing.
        let public_session = RelaySessionKey::public(url.clone());
        let planned = self.router.plan().reqs.contains_key(&public_session);
        if planned {
            if let Some(information) = information {
                self.nip11_information.insert(url.clone(), information);
            } else {
                // `None` means the service has no last-good authority for
                // this relay. An older reducer copy must not survive it.
                self.nip11_information.remove(&url);
            }
        } else {
            // A flight may complete after demand changed. Late evidence has
            // no current diagnostics owner and is never retained.
            self.nip11_information.remove(&url);
        }
        let mut effects = Vec::new();
        if self.connected_relays.contains(&public_session)
            && self.router.plan().reqs.contains_key(&public_session)
            && advertises_nip77 != Some(false)
        {
            if let Some(probe) = self.prober.begin_probe(&url) {
                effects.push(Effect::StartProbe(
                    url,
                    probe.sub_id,
                    probe.filter,
                    probe.initial_message_hex,
                ));
            }
        }
        // Capability evidence is itself observable diagnostics state; do
        // not wait for an unrelated query recompile/EOSE to publish it.
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        effects
    }

    /// `reason` is the one piece of information issue #506's CRITICAL fix
    /// restores across the pool->engine boundary. Ordinary (transient)
    /// disconnects keep EXACTLY today's behavior: the pool itself is already
    /// redialing on its own backoff schedule, and `Effect::EnsureRelay` here
    /// is an idempotent no-op nudge for that same worker. A
    /// `DisconnectReason::PermanentlyFailed` slot is different in kind: the
    /// transport pool has ALREADY retired that worker thread for good (see
    /// `nmp_transport::DisconnectReason::PermanentlyFailed`'s doc) -- it will
    /// never redial on its own, so re-issuing `EnsureRelay` unconditionally
    /// here would either be a silent no-op racing a wedged zombie (the
    /// pre-#506 bug) or, once the pool immediately reopens on ANY
    /// `ensure_open`, a tight redial loop against a relay that keeps
    /// rejecting the same way (a 401 busy-loop -- exactly what the fix must
    /// NOT introduce). So a permanent reason records a terminal degraded
    /// fact instead (reusing the same `transport_degraded` diagnostics field
    /// `on_relay_health` already owns) and stops short of `EnsureRelay`;
    /// every other reaction below (clearing attribution, suspending
    /// in-flight write lanes, dropping open reconciliations, clearing
    /// `connected_relays`) is identical for both reasons, because the relay
    /// is equally not-connected either way. Recovery for a permanently-failed
    /// relay is still possible afterward -- an explicit demand re-add or the
    /// write scheduler's own lane demand issues a FRESH `EnsureRelay`, which the pool
    /// grants a fresh generation for because its worker slot is already
    /// empty (`ensure_open` on an empty slot is indistinguishable from
    /// `close`-then-`ensure_open`) -- it is simply never AUTOMATIC.
    fn on_relay_disconnected(
        &mut self,
        handle: TransportRelayHandle,
        reported_session: RelaySessionKey,
        reason: DisconnectReason,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned() {
            // Exact (handle, session) match or nothing (#8): a delayed old
            // disconnect for a superseded generation, or one reported for a
            // session that no longer occupies this slot, must not tear down
            // the session that actually lives there now.
            if current != handle || session != reported_session {
                return effects;
            }
            // AUTH truth is a property of the exact connection generation
            // that earned it (#8) — it dies with the socket, unconditionally,
            // for every disconnect reason: the epoch is cancelled, protected
            // lanes park, and readiness is revoked.
            self.invalidate_auth_epoch(&session, false, &mut effects);
            self.attribution.clear_session(&session);
            self.suspend_disconnected_lanes(&session, &mut effects);
            // Negentropy (probe, live reconciliations, one-shot backfills)
            // is PUBLIC-session-only work (#8), so its teardown fires only
            // when the Public session itself dropped -- a protected
            // session's disconnect must not kill a reconciliation still
            // healthy on the URL's live Public socket.
            if session.access == AccessContext::Public {
                // Any reconciliation open against this relay dies with the
                // connection -- there is nothing left to `NEG-CLOSE` (the
                // socket is already gone), so this is a silent drop, not a
                // fallback REQ: the relay's own `Supported` verdict stays
                // cached, and the NEXT `recompile()`/reconnect naturally
                // re-opens whatever demand still wants this shape.
                self.neg_sessions
                    .retain(|_, neg| neg.relay != session.relay);
                // A one-shot negentropy backfill (`finish_neg_session`) that
                // was mid-flight on this relay will never EOSE now -- its
                // own socket is gone -- so `pending_backfills`/
                // `pending_neg_credit` (both keyed by the backfill's
                // relay-scoped `SubId`, whose `.0` is exactly this URL)
                // would otherwise orphan forever: the only other removal
                // site is EOSE-gated (`on_relay_frame`'s `EndOfStoredEvents`
                // arm). Coverage is not permanently lost -- a reconnect's
                // `recompile()` re-opens the live REQ and negentropy runs
                // again -- only the orphaned one-shot bookkeeping for THIS
                // attempt is dropped, exactly like `neg_sessions` above.
                self.pending_backfills
                    .retain(|sub_id| sub_id.0 != session.relay);
                self.pending_neg_credit
                    .retain(|sub_id, _| sub_id.0 != session.relay);
            }
            // Feeds `AcquisitionEvidence.sources[_].status`: this session is
            // no longer connected, but `ever_connected_relays` is untouched
            // -- a subsequent evidence computation reads `Disconnected`,
            // never `Connecting`, and any `reconciled_through` this session
            // already earned survives (the #49 "offline cached rows remain
            // usable" acceptance criterion -- watermark and link status are
            // deliberately orthogonal fields, never one enum).
            self.connected_relays.remove(&session);
            self.auth_probe_sessions.remove(&session);
            match reason {
                DisconnectReason::PermanentlyFailed => {
                    // #506: the pool already retired this worker for good --
                    // re-issuing `EnsureRelay` here would busy-loop against
                    // a relay that keeps saying no. Record the terminal
                    // degraded fact instead; recovery is only ever explicit
                    // (fresh demand or the write scheduler's lane demand).
                    let url = &session.relay;
                    self.transport_degraded = Some(format!(
                        "relay {url} permanently failed (authentication/authorization \
                         rejected) and will not automatically retry"
                    ));
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }
                DisconnectReason::Closed => {
                    // An INTENTIONAL close (`Pool::close`) must never
                    // resurrect the session (#8/ledger #18): the runtime's
                    // exact worker reconciliation just released it on
                    // purpose, and an unconditional `EnsureRelay` here would
                    // re-dial a still-planned session the instant it was
                    // reconciled away.
                }
                DisconnectReason::Error | DisconnectReason::ShuttingDown => {
                    // Transient drop: re-request the worker ONLY while the
                    // reducer still owns demand for exactly this session --
                    // a session no longer required must not be redialed
                    // merely because its old socket errored on the way out.
                    let still_required = self
                        .required_relay_workers()
                        .is_some_and(|required| required.contains(&session));
                    if still_required {
                        effects.push(Effect::EnsureRelay(session));
                    }
                }
            }
        }
        // Same reasoning as `on_relay_connected`: a link-status flip alone
        // must become observable via `EmitRows`.
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects.extend(self.schedule_ready(self.clock));
        effects
    }

    /// Consume a wire `OK` iff its event id belongs to the dedicated AUTH
    /// correlation namespace. At most one current correlation exists per
    /// admitted session. Retired ids need no tombstone set: ordinary publish
    /// structurally rejects kind:22242, so a retired/unknown AUTH id cannot
    /// exist in the durable-write correlation map and write fallback is a
    /// guaranteed no-op. Old-socket frames cannot cross a reconnect because
    /// transport handles are generation checked before this function.
    ///
    /// This is the ONE ready transition (#8): the FIRST exact-generation
    /// success records the session's planned REQs' attribution snapshots and
    /// replays them (the exact send `on_relay_connected` deliberately
    /// withheld for a protected session), wakes persisted `WaitingAuth`
    /// lanes, and refreshes evidence (`AwaitingAuth` -> `Requesting`); a
    /// duplicate OK for the same epoch does nothing (a second snapshot would
    /// poison the attribution FIFO with a send that never happened).
    fn finish_auth_ok(
        &mut self,
        session: &RelaySessionKey,
        mut state: AuthSessionState,
        event_id: EventId,
        status: bool,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        if !status {
            state.phase = AuthSessionPhase::Denied;
            self.auth_sessions.insert(session.clone(), state);
            self.refresh_all_handles(&mut effects);
            return effects;
        }

        state.phase = AuthSessionPhase::Ready { event_id };
        self.auth_ready_sessions
            .insert(session.clone(), state.epoch.handle);
        effects.push(Effect::ReleaseInitialRead(state.epoch.handle));
        if let Some(reqs) = self.router.plan().reqs.get(session).cloned() {
            for req in &reqs {
                self.attribution.record_send(
                    session,
                    &req.sub_id,
                    &req.filter,
                    req.absorbed.clone(),
                );
            }
            if !reqs.is_empty() {
                effects.push(Effect::Replay(session.clone(), reqs));
            }
        }
        self.auth_sessions.insert(session.clone(), state);
        effects.extend(self.wake_relay_lanes(session, true));
        self.refresh_all_handles(&mut effects);
        self.refresh_all_histories(&mut effects);
        effects
    }

    fn on_auth_ok(
        &mut self,
        session: &RelaySessionKey,
        event_id: EventId,
        status: bool,
    ) -> Option<Vec<Effect>> {
        let epoch = self.auth_sessions.get(session)?.epoch.clone();
        if !self.exact_current_auth_epoch(&epoch) {
            return None;
        }
        let mut state = self.auth_sessions.remove(session)?;
        let current_event_id = match &mut state.phase {
            AuthSessionPhase::AwaitingOk { event_id: current } if *current == event_id => *current,
            AuthSessionPhase::AwaitingSend {
                token: _,
                event_id: current,
                early_ok,
            } if *current == event_id => {
                if early_ok.is_none() {
                    *early_ok = Some(status);
                }
                self.auth_sessions.insert(session.clone(), state);
                return Some(Vec::new());
            }
            AuthSessionPhase::Ready { event_id: current } if *current == event_id => {
                self.auth_sessions.insert(session.clone(), state);
                return Some(Vec::new());
            }
            _ => {
                self.auth_sessions.insert(session.clone(), state);
                return None;
            }
        };

        Some(self.finish_auth_ok(session, state, current_event_id, status))
    }

    // ---- inbound relay frame: EVENT/EOSE parsed here (D/E own OK/CLOSED/
    // NOTICE/AUTH/COUNT/NEG-*) --------------------------------------------

    fn ingest_relay_events(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
        effects: &mut Vec<Effect>,
    ) {
        if events.is_empty() {
            return;
        }
        let relay_list_authors: Vec<_> = events
            .iter()
            .filter_map(|(event, _)| (event.kind == nostr::Kind::RelayList).then_some(event.pubkey))
            .collect();
        // The per-session diagnostics counter (`events_by_session_kind`) is
        // bumped at the frame sites (`on_relay_frame`/`on_relay_frames`),
        // where the exact physical session is still known — a
        // `RelayObserved` carries only the URL, which cannot distinguish
        // access contexts (#8).
        match self.resolver.ingest_observed_detailed(events) {
            Err(error) => self.degrade_store(error, effects),
            Ok(ingest) => {
                // Recompute this up front from the embedded `committed.delta`
                // before it moves into `apply_committed_mutation_with` below:
                // it drives the diagnostics-vs-recompile choice, which is a
                // genuinely relay-specific concern (event counters need a
                // diagnostics beat even when the shared apply took the
                // exact/no-recompile path) and therefore stays outside the
                // one shared refresh-vs-apply decision rather than
                // re-implementing it.
                let demand_changed = !ingest.committed.delta.is_empty();
                let satisfied_pending = !ingest.satisfied_intents.is_empty();
                for (intent_id, canonical) in ingest.satisfied_intents {
                    if let Some((receipt_id, pending)) = self
                        .pending
                        .iter_mut()
                        .find(|(_, pending)| pending.intent_id == Some(intent_id))
                    {
                        pending.already_signed = true;
                        pending.sign_request_in_flight = false;
                        let receipt_id = *receipt_id;
                        self.on_signed(receipt_id, canonical, effects);
                    }
                }
                let mut directory_changed = false;
                for author in relay_list_authors {
                    directory_changed |= self.ingest_relay_list_winner(author, effects);
                }

                // Ordinary committed rows do not change the active demand or
                // router plan. Avoid rebuilding it on every EVENT batch; a
                // resolver atom delta or an actual NIP-65 directory change is
                // the evidence that routing may differ.
                if !(demand_changed || directory_changed) {
                    // Event counters are diagnostics facts even when the
                    // demand/router plan is unchanged. Preserve the prior
                    // observable update without paying a full router compile.
                    effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
                }

                // A demand/directory change may alter the capped source plan
                // and therefore evidence for otherwise-unrelated handles;
                // keep that path broad. The dominant ordinary-ingest path is
                // exact: refresh only subscriptions whose root filter matches
                // a changed row (or whose shared projection shape changed).
                // `directory_changed`/`satisfied_pending` are relay-only
                // evidence the resolver's own `delta` never carries, so they
                // ride in as explicit force flags on the SAME shared apply
                // `apply_committed_mutation` uses for every other committed-
                // mutation door, instead of re-deciding refresh-vs-apply here.
                self.apply_committed_mutation_with(
                    ingest.committed,
                    directory_changed,
                    directory_changed || satisfied_pending,
                    effects,
                );
            }
        }
    }

    fn on_relay_frames(
        &mut self,
        frames: Vec<(TransportRelayHandle, RelaySessionKey, RelayFrame)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let mut events = Vec::new();
        for (handle, reported_session, frame) in frames {
            match frame.into_event() {
                Ok(event) => {
                    let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned()
                    else {
                        self.ingest_relay_events(std::mem::take(&mut events), &mut effects);
                        continue;
                    };
                    // BOTH halves must match (#8): a frame carrying a stale
                    // generation OR a session that no longer occupies this
                    // slot is dropped exactly — never re-attributed to the
                    // slot's current occupant.
                    if current != handle || session != reported_session {
                        self.ingest_relay_events(std::mem::take(&mut events), &mut effects);
                        continue;
                    }
                    *self
                        .events_by_session_kind
                        .entry(session.clone())
                        .or_default()
                        .entry(event.kind.as_u16())
                        .or_insert(0) += 1;
                    events.push((event, RelayObserved::new(session.relay, self.clock)));
                }
                Err(frame) => {
                    self.ingest_relay_events(std::mem::take(&mut events), &mut effects);
                    effects.extend(self.on_relay_frame(handle, reported_session, frame));
                }
            }
        }
        self.ingest_relay_events(events, &mut effects);
        effects
    }

    fn on_relay_frame(
        &mut self,
        handle: TransportRelayHandle,
        reported_session: RelaySessionKey,
        frame: RelayFrame,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        let msg = frame.into_message();
        let Some((current, session)) = self.slot_to_relay.get(&handle.slot).cloned() else {
            return effects; // frame from a slot we never saw RelayConnected for.
        };
        // BOTH halves must match (#8): the exact current generation AND the
        // exact session the reducer connected on this slot. A wrong-session
        // frame (however it was produced) must never consume another
        // session's attribution FIFO, coverage credit, probe, or write ack.
        if current != handle || session != reported_session {
            return effects;
        }

        match msg {
            RelayMessage::Event { event, .. } => {
                let event = event.into_owned();
                *self
                    .events_by_session_kind
                    .entry(session.clone())
                    .or_default()
                    .entry(event.kind.as_u16())
                    .or_insert(0) += 1;
                let observed = RelayObserved::new(session.relay.clone(), self.clock);
                self.ingest_relay_events(vec![(event, observed)], &mut effects);
            }
            RelayMessage::EndOfStoredEvents(sub_id) => {
                let wire_id = sub_id.as_str();
                let attributed = self
                    .attribution
                    .attribute_eose(&session, wire_id, self.clock);
                for (key, interval) in attributed {
                    if let Some(atom) = self.attribution.shape_of(key) {
                        // Coverage rows stay keyed (context-hashed key,
                        // relay URL) — the access distinction already lives
                        // inside the key's own hash, so the store door takes
                        // the session's relay.
                        if let Err(e) = self.resolver.store_mut().record_coverage(
                            &atom,
                            &session.relay,
                            interval,
                        ) {
                            // Persisting a coverage watermark failed (issue
                            // #122): degrade rather than panic. The
                            // in-memory `Effect::RecordCoverage` is skipped
                            // too — no watermark is claimed that did not
                            // durably land.
                            self.degrade_store(e, &mut effects);
                            continue;
                        }
                        effects.push(Effect::RecordCoverage(key, session.relay.clone(), interval));
                    }
                }
                // A watermark advancing can flip a handle's
                // AcquisitionEvidence (a source's `reconciled_through`) even
                // with no new rows at all — refresh so that becomes
                // observable via EmitRows, same as an ingest.
                self.refresh_all_handles(&mut effects);
                self.refresh_all_histories(&mut effects);
                // Same watermark advance can also flip the diagnostic
                // surface's own per-(filter, relay) coverage even though
                // this arm never calls `recompile()` (M5 plan §1.2 step 3:
                // "after the Event/EOSE ingest arms ... coverage change
                // points").
                effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));

                // A one-shot negentropy backfill REQ (`finish_neg_session`)
                // has nothing further to prove once it EOSEs -- close it so
                // it does not linger as a subscription the router's own
                // demand-diffing never knew existed, and -- if it was
                // deferring a reconciliation's coverage credit -- THIS is
                // the moment the backfilled events are proven ingested
                // (EVENT precedes EOSE, NIP-01), so it is now safe to credit
                // (ledger #7: never before this point).
                if let Some(resolved) = self.attribution.sub_id_for_wire(&session, wire_id) {
                    if self.pending_backfills.remove(&resolved) {
                        effects.push(Effect::Wire(WireDelta {
                            ops: vec![(session.clone(), vec![WireOp::Close(resolved.clone())])],
                        }));
                    }
                    if let Some(original_sub_id) = self.pending_neg_credit.remove(&resolved) {
                        self.credit_neg_coverage(&original_sub_id, &session.relay, &mut effects);
                    }
                }
            }
            RelayMessage::Ok {
                event_id,
                status,
                message,
            } => {
                // AUTH-OK correlation is checked BEFORE durable-write ACK
                // correlation (#8): the two namespaces are structurally
                // disjoint (ordinary publish rejects kind:22242), so a hit
                // here can never starve a real write ack, and a miss falls
                // through to the ordinary write path unchanged.
                if let Some(auth_effects) = self.on_auth_ok(&session, event_id, status) {
                    effects.extend(auth_effects);
                } else {
                    self.handle_write_ack(
                        event_id,
                        status,
                        message.into_owned(),
                        &session,
                        &mut effects,
                    );
                }
            }
            RelayMessage::Auth { challenge } => {
                effects.extend(self.on_auth_challenge(handle, session, challenge.into_owned()));
            }
            RelayMessage::Closed { message, .. }
                if matches!(
                    message.split_once(':').map(|(prefix, _)| prefix),
                    Some("auth-required" | "restricted")
                ) =>
            {
                effects.extend(self.on_auth_restricted(handle, session));
            }
            RelayMessage::NegMsg {
                subscription_id,
                message,
            } => {
                // Negentropy is PUBLIC-session-only in this unit (#8): the
                // probe and every reconciliation were opened on the Public
                // session, so a NEG frame arriving on a protected session
                // could only be a foreign/confused reply — it must not
                // resolve the Public probe or step a Public reconciliation.
                if session.access != AccessContext::Public {
                    return effects;
                }
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_msg(&session.relay, wire_id).is_some() {
                    // Capability probe succeeded -- the verdict is now
                    // cached (`Prober::probed`). Nothing further to do here:
                    // the NEXT `recompile()` (triggered by any future demand
                    // change) is what actually routes a broad filter for
                    // this relay onto negentropy -- see the builder report's
                    // scoping note on already-open subs at probe time.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&session, wire_id) {
                    self.step_neg_session(
                        sub_id,
                        session.relay.clone(),
                        message.as_ref(),
                        &mut effects,
                    );
                }
                // An unrecognized wire id is an untrusted-network fact
                // (stale/foreign sub), never a panic -- silently ignored,
                // same discipline as `handle_write_ack`'s unknown-OK case.
            }
            RelayMessage::NegErr {
                subscription_id, ..
            } => {
                // Same PUBLIC-session-only gate as `NegMsg` above (#8): a
                // protected session's NEG-ERR must not classify the URL as
                // Unsupported or tear a Public reconciliation down to REQ.
                if session.access != AccessContext::Public {
                    return effects;
                }
                let wire_id = subscription_id.as_str();
                if self.prober.on_neg_unsupported(&session.relay, wire_id) {
                    // Probe classified Unsupported; cached, never re-probed.
                } else if let Some(sub_id) = self.attribution.sub_id_for_wire(&session, wire_id) {
                    if let Some(neg) = self.neg_sessions.remove(&sub_id) {
                        self.neg_session_fallback_to_req(sub_id, neg, &mut effects);
                    }
                }
            }
            // Closed (non-auth) / Notice / Count remain separate protocol
            // facts.
            _ => {}
        }
        effects
    }

    // ---- shared recompile + row-refresh plumbing -------------------------

    /// Recompile the router from the resolver's CURRENT demand, record any
    /// newly-sent REQs' attribution snapshots, and push `Effect::Wire` for
    /// whatever op actually changed on the wire -- EXCEPT a broad
    /// (unlimited) `Req` for a relay this reducer has PROVEN supports
    /// NIP-77 (`Prober::probed`), which is routed negentropy-first instead
    /// (plan §6 E: "negentropy-FIRST for a probed relay + broad filter; REQ
    /// fallback otherwise"). Ledger #8 is structural here, not a runtime
    /// `if` bolted on top: `open_neg_session` is the ONLY call site that can
    /// produce an `Effect::NegOpen`, and it can only be reached by first
    /// obtaining a `ProbedRelay` from `Prober::probed` -- an unprobed relay
    /// has no token to pass, so its `Req` arm always falls through to the
    /// plain-REQ branch below, every time.
    fn recompile(&mut self, effects: &mut Vec<Effect>) {
        #[cfg(test)]
        self.router_compiles
            .set(self.router_compiles.get().saturating_add(1));
        self.sync_discovery(effects);
        let demand = self.resolver.active_demand();
        self.attribution.observe_demand(demand.iter());
        // Finding E3 (epic #507): prune `shape_by_key` against the SAME
        // `demand` just observed above, plus every key still `absorbed` by
        // an outstanding attribution snapshot (see `prune_shapes`'s own
        // doc for why the latter is required) -- mirrors the
        // `nip11_information.retain(..)` a few lines below, in the same
        // function, against the same kind of "current authoritative set"
        // (`planned`/`demand`) recompile just established.
        self.attribution.prune_shapes(demand.iter());
        let admitted_demand = self.admit_projected_routing_evidence(&demand);
        let wire_delta: WireDelta =
            self.router
                .compile(&admitted_demand, self.directory.as_ref(), self.cap);
        let planned = &self.router.plan().reqs;
        // NIP-11 evidence is retained for any URL that appears as SOME
        // planned session's relay (#8): the document is per-URL evidence,
        // and a URL planned only under a protected session still keeps its
        // document current for the moment its Public session is planned too.
        self.nip11_information
            .retain(|relay, _| planned.keys().any(|session| &session.relay == relay));
        // Finding E4 (epic #507): `events_by_session_kind` is bumped once
        // per inbound EVENT (`on_relay_frame`/`on_relay_frames`) but was
        // never pruned when a session permanently left the plan/directory,
        // growing unbounded across relay churn. `diagnostics::build` only
        // ever reads it via `.get(session)` for `session in
        // &diag.per_session`, and `diag.per_session` is itself built
        // straight off `plan.reqs` (`nmp-router`'s `diag::build`) -- i.e.
        // exactly `planned` here -- so no live reader ever consults an
        // entry outside this set. Safe to prune against the SAME
        // "still-planned" key set as `nip11_information` just above.
        self.events_by_session_kind
            .retain(|session, _| planned.contains_key(session));
        // Protected REQs stay parked until the exact current AUTH epoch is
        // ready, but the relay worker must already exist so the server can
        // deliver the challenge that makes readiness possible. Plan keys are
        // unique, so this emits at most one idempotent acquisition edge per
        // current protected session on each recompile. Exact runtime worker
        // reconciliation still owns withdrawal and closes the worker as soon
        // as the final read/write owner disappears.
        effects.extend(
            planned
                .keys()
                .filter(|session| {
                    session.access != AccessContext::Public
                        && !self.auth_ready_sessions.contains_key(*session)
                })
                .cloned()
                .map(Effect::EnsureRelay),
        );
        // `router.compile()` above ALWAYS finalizes `prev_plan`/`last_diag`
        // for the full current demand, regardless of whether anything
        // actually changed on the wire (see `Router::compile`'s own body) —
        // so diagnostics is pushed unconditionally here (M5 plan §1.2 step
        // 3: "push it at the end of recompile()"), even on the early return
        // below for a no-op wire delta.
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        if wire_delta.ops.is_empty() {
            return;
        }

        let mut kept: Vec<(RelaySessionKey, Vec<WireOp>)> = Vec::new();
        for (session, ops) in &wire_delta.ops {
            // A PROTECTED session's ops are dropped from the wire delta
            // entirely until its exact current generation has completed AUTH
            // (#8): its REQs park (the AUTH reducer's ready transition,
            // `finish_auth_ok`, replays the full planned set on readiness,
            // so nothing is lost), and no CLOSE is needed pre-auth — nothing
            // was ever sent on that socket for this plan to withdraw.
            if session.access != AccessContext::Public
                && !self.auth_ready_sessions.contains_key(session)
            {
                continue;
            }
            let mut kept_ops: Vec<WireOp> = Vec::new();
            for op in ops {
                match op {
                    WireOp::Req(sub_id, filter) => {
                        let absorbed = self
                            .router
                            .plan()
                            .reqs
                            .get(session)
                            .and_then(|reqs| reqs.iter().find(|r| &r.sub_id == sub_id))
                            .map(|r| r.absorbed.clone())
                            .unwrap_or_default();

                        // "Small exact result" (a `limit`) always stays REQ
                        // -- a bounded, terminating fetch is not what
                        // negentropy set-reconciliation is for, and `limit`
                        // poisons coverage attribution regardless (ruling
                        // §3), so there is nothing negentropy-first would
                        // buy it. Negentropy-first is additionally PUBLIC-
                        // session-only (#8): the probe verdict was earned on
                        // the unauthenticated socket and proves nothing
                        // about an authenticated session's view.
                        let broad = filter.limit.is_none();
                        match (
                            broad && session.access == AccessContext::Public,
                            self.prober.probed(&session.relay),
                        ) {
                            (true, Some(probed)) => {
                                self.open_neg_session(
                                    probed,
                                    sub_id.clone(),
                                    filter.clone(),
                                    absorbed,
                                    effects,
                                );
                            }
                            _ => {
                                self.attribution
                                    .record_send(session, sub_id, filter, absorbed);
                                kept_ops.push(op.clone());
                            }
                        }
                    }
                    WireOp::Close(sub_id) => {
                        self.neg_sessions.remove(sub_id);
                        kept_ops.push(op.clone());
                    }
                }
            }
            if !kept_ops.is_empty() {
                kept.push((session.clone(), kept_ops));
            }
        }

        if !kept.is_empty() {
            effects.push(Effect::Wire(WireDelta { ops: kept }));
        }
    }

    /// Compile the resolver's current (possibly staged-history) demand into
    /// an isolated plan. A history advance changes only the outer time
    /// window of an already-live descriptor, so every discovery dependency
    /// is already represented by the initial session; shadow planning never
    /// needs to mutate the widen-only discovery subscription.
    fn history_shadow_plan(&self) -> RelayPlan {
        let admitted = self
            .resolver
            .active_demand()
            .into_iter()
            .map(|mut atom| {
                atom.routing_evidence
                    .retain(|evidence| self.admission.admits_discovered(&evidence.relay));
                atom
            })
            .collect();
        let mut router = Router::new(
            DiscoveryKinds::default(),
            RuleRegistry::default_widen_only(),
        );
        let _ = router.compile(&admitted, self.directory.as_ref(), self.cap);
        router.plan().clone()
    }

    /// Gate every network-sourced selector hint/provenance URL before it
    /// can become a router candidate. Operator-configured lanes remain
    /// trusted and bypass this path, matching kind:10002 admission policy.
    fn admit_projected_routing_evidence(
        &mut self,
        demand: &BTreeSet<ContextualAtom>,
    ) -> BTreeSet<ContextualAtom> {
        let mut rejected_now = BTreeSet::new();
        let admitted = demand
            .iter()
            .cloned()
            .map(|mut atom| {
                let atom_selection = atom.filter.hash();
                atom.routing_evidence.retain(|evidence| {
                    let admitted = self.admission.admits_discovered(&evidence.relay);
                    if !admitted {
                        rejected_now.insert((atom_selection, evidence.clone()));
                    }
                    admitted
                });
                atom
            })
            .collect();
        let newly_rejected = rejected_now
            .difference(&self.rejected_projected_evidence)
            .count() as u64;
        self.discovered_private_relays_rejected = self
            .discovered_private_relays_rejected
            .saturating_add(newly_rejected);
        self.rejected_projected_evidence = rejected_now;
        admitted
    }

    /// The self-bootstrapping outbox (M5, `docs/known-gaps.md`'s
    /// "RelayDirectory" gap): keep an internal kind:10002 discovery
    /// subscription open covering EVERY author current demand has EVER
    /// referenced whose write relays `self.directory` didn't know yet at the
    /// time -- never a permanent/whole-graph scan (still bounded by "every
    /// author this session has actually demanded content for"). Called at
    /// the top of every `recompile` (i.e. on every subscribe/unsubscribe/
    /// re-root/ingest).
    ///
    /// WIDEN-ONLY (`docs/known-gaps.md`'s kind:10002 over-fetch finding: 7112
    /// events received against a 39-author resolved set, root-caused to THIS
    /// function -- see the finding's investigation notes): a newly-demanded
    /// author with unknown relays widens the subscription; an author whose
    /// relays just became known is deliberately left IN the filter rather
    /// than dropped. Reopening on every shrink was the actual bug -- an
    /// author leaving `needed` the moment their kind:10002 resolves used to
    /// tear down and reopen the ENTIRE subscription (dropping that one
    /// author from a fresh, differently-shaped filter), and to a NIP-01
    /// relay an overwriting Req on an already-open sub-id is
    /// indistinguishable from a brand-new subscription: it replies with a
    /// full EOSE replay of every event still matching the new filter. Over N
    /// authors resolving one at a time that is a triangular-number amount of
    /// redelivered events (N+(N-1)+...+1), not O(N) -- exactly the
    /// mechanism behind the 7112-for-39 finding. Leaving a resolved author
    /// in the filter a while longer is widen-safe (matches(wider) ⊇
    /// matches(narrower), the same proof obligation `nmp_router::coalesce`'s
    /// `AuthorUnion` rule already carries) -- it can only mean a few extra,
    /// already-known kind:10002 deliveries for that author, never a
    /// structural over-fetch. The subscription is only ever torn down when
    /// `needed` goes fully empty (every demanded author has resolved, or
    /// none are demanded at all) -- at that point there is nothing left this
    /// discovery sub is for, so it closes rather than idling forever.
    ///
    /// Deliberately reuses the ordinary resolver subscribe/unsubscribe
    /// machinery rather than hand-rolling a parallel subscription system:
    /// the discovery atom this produces (`kinds:[10002], authors:{covered}`)
    /// is just another entry in `resolver.active_demand()`, so the router's
    /// EXISTING discovery-kind eligibility is what routes it to the
    /// configured indexers -- no router-side change was needed for that half
    /// at all. A content atom for an author with no known write relays
    /// simply routes nowhere in the meantime (never an indexer fallback --
    /// "indexers are never a content fallback").
    fn sync_discovery(&mut self, effects: &mut Vec<Effect>) {
        let needed: BTreeSet<PubkeyHex> = self
            .resolver
            .active_demand()
            .into_iter()
            .filter_map(|atom| atom.filter.authors)
            .flatten()
            // NOT `write_relays(..).is_empty()`: that collapses "known,
            // declares zero write relays" into the same signal as "never
            // resolved", which kept a discovery subscription open FOREVER
            // for an author who genuinely has no write relays (ledger #20).
            // `knows_write_relays` distinguishes the two; only a genuinely
            // unresolved author still needs discovery.
            .filter(|author| !self.directory.knows_write_relays(author))
            .collect();

        if needed.is_empty() {
            if self.discovery_handle.is_none() && self.discovery_authors.is_empty() {
                return; // already closed -- nothing to do.
            }
            // Every previously-needed author has resolved (or nothing was
            // ever demanded): nothing left for this sub to cover, so close
            // it. Its `Drop` impl only ENQUEUES the withdrawal; there is
            // nothing to replace it with, so flush explicitly.
            self.discovery_handle = None;
            self.discovery_authors = BTreeSet::new();
            let _ = self.resolver.poll_pending_drops();
            return;
        }

        if needed.is_subset(&self.discovery_authors) {
            // Nothing NEW to cover -- leave the existing subscription
            // exactly as-is, even though it may now be wider than strictly
            // required (see this fn's doc: that's the whole point).
            return;
        }

        // Widen: union in whatever's newly needed and reopen with the
        // WIDENED set. Its `Drop` impl only ENQUEUES the old withdrawal;
        // `resolver.subscribe`'s own drain-on-entry flushes it before
        // building the new atom.
        self.discovery_authors = self.discovery_authors.union(&needed).cloned().collect();
        self.discovery_handle = None;
        let query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(Binding::Literal(self.discovery_authors.clone())),
            ..Filter::default()
        });
        // Building the internal discovery subscription can read the store.
        // On a persistence failure (issue #122) degrade to read-only and
        // open no discovery sub rather than panic.
        match self.resolver.subscribe(query) {
            Ok((handle, _delta)) => self.discovery_handle = Some(handle),
            Err(e) => self.degrade_store(e, effects),
        }
    }

    /// After ingesting a possible kind:10002 event for `author`, re-read the
    /// store's CURRENT winning relay-list event for them -- never trust the
    /// just-arrived frame directly. `EventStore::query` only ever returns
    /// the current replaceable-event winner (`nmp-store`'s own contract), so
    /// this is correct regardless of cross-relay arrival order: a stale/
    /// older copy that already lost the replaceable race at `insert` time
    /// can never overwrite the directory with worse data than what the
    /// store itself considers authoritative.
    fn ingest_relay_list_winner(
        &mut self,
        author: nostr::PublicKey,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let filter = ConcreteFilter {
            kinds: Some(BTreeSet::from([NIP65_RELAY_LIST_KIND])),
            authors: Some(BTreeSet::from([author.to_hex()])),
            ..ConcreteFilter::default()
        };
        // Re-reading the store's current relay-list winner can fail on I/O
        // (issue #122): degrade to read-only rather than panic. The
        // directory simply isn't updated for this author on this frame.
        let winner = match self.resolver.store().query(&filter.to_nostr()) {
            Ok(rows) => rows.into_iter().next(),
            Err(e) => {
                self.degrade_store(e, effects);
                return false;
            }
        };
        let Some(winner) = winner else {
            return false;
        };
        // Relay admission (issue #121): these relays are DISCOVERED — parsed
        // straight off a network-sourced (validly-signed, but untrusted-
        // content) kind:10002. Gate them on host classification + the
        // operator's opt-in local allowlist BEFORE they become routable
        // `Nip65Write`/`Nip65Read` lanes. A rejected relay never enters the
        // directory, so it never becomes a router candidate and never reaches
        // `pool.ensure_open` — the SSRF / forced-Tor path is closed
        // structurally, not filtered downstream.
        //
        // FORWARD GUARD: this is currently the SOLE network-discovery path
        // into the relay directory. ANY future network-sourced relay ingest —
        // a kind:10050 DM-inbox list, nprofile/nevent relay hints, a
        // provenance "seen here" lane, etc. — MUST route its parsed relays
        // through `self.admission.filter_discovered(..)` before calling
        // `directory.ingest_*`, or the structural exclusion proven here is
        // silently lost for that new source. Discovery is untrusted;
        // operator config (the `LiveDirectory` builder lanes) is not and is
        // deliberately NOT gated here.
        let (write_relays, write_rejected) = self
            .admission
            .filter_discovered(parse_nip65_write_relays(&winner.event));
        let (read_relays, read_rejected) = self
            .admission
            .filter_discovered(parse_nip65_read_relays(&winner.event));
        self.discovered_private_relays_rejected = self
            .discovered_private_relays_rejected
            .saturating_add(write_rejected + read_rejected);
        let author = author.to_hex();
        let before_known = self.directory.knows_write_relays(&author);
        let before_write = self.directory.write_relays(&author);
        let before_read = self.directory.read_relays(&author);
        self.directory
            .ingest_write_relays(author.clone(), write_relays);
        self.directory
            .ingest_read_relays(author.clone(), read_relays);
        before_known != self.directory.knows_write_relays(&author)
            || before_write != self.directory.write_relays(&author)
            || before_read != self.directory.read_relays(&author)
    }

    /// Open a real negentropy reconciliation for `filter` against `probed`
    /// (plan §6 E). Reads the local store's own current holdings for the
    /// (window-erased) shape to seed the `Reconciler`, records the send-time
    /// attribution snapshot exactly as a plain REQ would (ruling §2: NEG
    /// runs unfloored/unlimited, so `neg_filter` below IS that snapshot's
    /// filter, with no separate floor/until/limited fields to keep in
    /// sync), and emits the `NegOpen` effect.
    fn open_neg_session(
        &mut self,
        probed: ProbedRelay,
        sub_id: SubId,
        filter: ConcreteFilter,
        absorbed: BTreeSet<CoverageKey>,
        effects: &mut Vec<Effect>,
    ) {
        // REQ and NEG-OPEN share ONE subscription-id namespace on the wire
        // (NIP-77): release whatever this `sub_id` may already mean to the
        // relay (a live plain REQ from before this relay was known
        // `Supported`, or nothing at all -- closing an id the relay never
        // opened is a harmless no-op) before reopening it as a NEG session.
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(
                RelaySessionKey::public(probed.url().clone()),
                vec![WireOp::Close(sub_id.clone())],
            )],
        }));

        let neg_filter = ConcreteFilter {
            since: None,
            until: None,
            limit: None,
            ..filter
        };
        // Seeding the reconciler reads the local store's holdings for this
        // shape. On an I/O failure (issue #122) degrade to read-only and do
        // not open the session rather than panic — the `Close` pushed above
        // still stands, so the sub-id is simply released.
        let local_rows = match self.resolver.store().query(&neg_filter.to_nostr()) {
            Ok(rows) => rows,
            Err(e) => {
                self.degrade_store(e, effects);
                return;
            }
        };
        let local_ids: Vec<(u64, EventId)> = local_rows
            .into_iter()
            .map(|se| (se.event.created_at.as_secs(), se.event.id))
            .collect();
        let (reconciler, initial_hex) = Reconciler::open(&local_ids);

        self.attribution.record_send(
            &RelaySessionKey::public(probed.url().clone()),
            &sub_id,
            &neg_filter,
            absorbed.clone(),
        );
        self.neg_sessions.insert(
            sub_id.clone(),
            NegSession {
                relay: probed.url().clone(),
                filter: neg_filter.clone(),
                absorbed,
                started_at: self.clock,
                reconciler,
            },
        );
        effects.push(Effect::NegOpen(probed, sub_id, neg_filter, initial_hex));
    }

    /// Drive one inbound `NEG-MSG` round for `sub_id`'s live session, if any
    /// (a frame for a sub this reducer isn't tracking is an untrusted-
    /// network fact, silently ignored -- same discipline as
    /// `handle_write_ack`'s unknown-`OK` case).
    fn step_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        message_hex: &str,
        effects: &mut Vec<Effect>,
    ) {
        let Some(session) = self.neg_sessions.get_mut(&sub_id) else {
            return;
        };
        let step = session.reconciler.step(message_hex);
        match step {
            Ok(NegStep::Continue(next_hex)) => {
                effects.push(Effect::NegMsg(relay, sub_id, next_hex));
            }
            Ok(NegStep::Done(need_ids)) => {
                let session = self
                    .neg_sessions
                    .remove(&sub_id)
                    .expect("just matched via get_mut above -- still present");
                self.finish_neg_session(sub_id, relay, session, need_ids, effects);
            }
            Err(_) => {
                // A malformed/unexpected reconcile payload from an
                // untrusted relay: abandon this reconciliation and fall
                // back to a plain REQ for the same filter -- the same
                // recovery path as the liveness-deadline/NEG-ERR cases,
                // never a silent read-gap.
                if let Some(session) = self.neg_sessions.remove(&sub_id) {
                    self.neg_session_fallback_to_req(sub_id, session, effects);
                }
            }
        }
    }

    /// Reconciliation completed (plan §6 E, the ruling's "feed a NEG-DONE
    /// the same way [as EOSE]"). Releases the session's sub-id, backfills
    /// whatever ids negentropy proved we are missing through the ordinary
    /// REQ/EOSE/ingest pipeline (never a separate ingest path), and reopens
    /// the same sub-id as a plain, live REQ floored at "now" -- negentropy
    /// is a point-in-time backlog sync, not a persistent subscription
    /// (ruling §3), so the relay's ongoing live tail still needs an open
    /// REQ once the backlog is settled.
    ///
    /// Evidence crediting (ledger #7) is NOT immediate when a backfill is
    /// needed: recording a reconciled watermark before the backfilled events
    /// are actually ingested would attach evidence to a store
    /// that is still, transiently, missing precisely the events negentropy
    /// just proved are missing.
    /// `pending_neg_credit` defers the credit to the backfill sub's OWN
    /// EOSE (`on_relay_frame`), by which point the events are already
    /// ingested (EVENT frames precede EOSE, NIP-01). An empty `need_ids`
    /// has nothing to wait for, so it credits right away.
    fn finish_neg_session(
        &mut self,
        sub_id: SubId,
        relay: RelayUrl,
        session: NegSession,
        need_ids: BTreeSet<EventId>,
        effects: &mut Vec<Effect>,
    ) {
        let NegSession {
            filter, absorbed, ..
        } = session;
        effects.push(Effect::NegClose(relay.clone(), sub_id.clone()));

        if need_ids.is_empty() {
            self.credit_neg_coverage(&sub_id, &relay, effects);
        } else {
            let backfill = ConcreteFilter {
                ids: Some(need_ids.iter().map(|id| id.to_hex()).collect()),
                ..ConcreteFilter::default()
            };
            // An id-targeted one-shot backfill fetch, not itself tied to
            // any live Demand (#106): no `authors` binding at all, so
            // `Public`/`Public` is the exact context `Demand::from_filter`'s
            // static default would assign an authorless filter -- and this
            // sub carries no coverage credit of its own anyway (`absorbed`
            // is empty below; the credit it unlocks is `sub_id`'s, via
            // `pending_neg_credit`).
            let backfill_sub = SubId::for_wire(
                relay.clone(),
                &backfill,
                &SourceAuthority::Public,
                AccessContext::Public,
            );
            self.pending_backfills.insert(backfill_sub.clone());
            self.pending_neg_credit
                .insert(backfill_sub.clone(), sub_id.clone());
            // No coverage credit of its OWN for this one-shot id-set fetch
            // -- `absorbed` is deliberately empty; it targets exactly the
            // ids negentropy already proved, it is not itself a proof over
            // any atom's shape (the credit it unlocks is `sub_id`'s, via
            // `pending_neg_credit` above).
            self.attribution.record_send(
                &RelaySessionKey::public(relay.clone()),
                &backfill_sub,
                &backfill,
                BTreeSet::new(),
            );
            effects.push(Effect::Wire(WireDelta {
                ops: vec![(
                    RelaySessionKey::public(relay.clone()),
                    vec![WireOp::Req(backfill_sub, backfill)],
                )],
            }));
        }

        let live_tail = ConcreteFilter {
            since: Some(self.clock.as_secs()),
            ..filter
        };
        self.attribution.record_send(
            &RelaySessionKey::public(relay.clone()),
            &sub_id,
            &live_tail,
            absorbed,
        );
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(
                RelaySessionKey::public(relay),
                vec![WireOp::Req(sub_id, live_tail)],
            )],
        }));
    }

    /// Attribute coverage for `sub_id` through the EXACT SAME
    /// `AttributionState::attribute_eose` call the real EOSE path uses --
    /// no second coverage mechanism, whether called directly (no backfill
    /// needed) or from `on_relay_frame`'s EOSE arm once a deferred backfill
    /// lands (`pending_neg_credit`).
    fn credit_neg_coverage(&mut self, sub_id: &SubId, relay: &RelayUrl, effects: &mut Vec<Effect>) {
        // Negentropy sessions are opened exclusively on the Public session
        // (#8), so their credit resolves through the same Public-session
        // attribution key `open_neg_session` recorded under.
        let attributed = self.attribution.attribute_eose(
            &RelaySessionKey::public(relay.clone()),
            &wire_sub_id_string(sub_id),
            self.clock,
        );
        for (key, interval) in attributed {
            if let Some(shape) = self.attribution.shape_of(key) {
                if let Err(e) = self
                    .resolver
                    .store_mut()
                    .record_coverage(&shape, relay, interval)
                {
                    // Coverage-watermark persistence failed (issue #122):
                    // degrade to read-only, claim no watermark that did not
                    // land, and do not panic.
                    self.degrade_store(e, effects);
                    continue;
                }
                effects.push(Effect::RecordCoverage(key, relay.clone(), interval));
            }
        }
        self.refresh_all_handles(effects);
        self.refresh_all_histories(effects);
    }

    /// Abandon a live reconciliation and fall back to a plain REQ for the
    /// SAME (unfloored/unlimited) filter -- shared by the liveness-deadline
    /// sweep (`tick`), an inbound `NEG-ERR`, and a malformed reconcile
    /// payload (`step_neg_session`'s `Err` arm). The abandoned session's own
    /// attribution snapshot is left outstanding rather than popped: the
    /// fallback REQ's EOSE will credit it via the SAME intersection rule an
    /// overwriting REQ already relies on (both snapshots carry the
    /// identical `absorbed`/`floor`/`until`/`limited` fields, since both
    /// derive from `session.filter`), so pop order does not matter here.
    fn neg_session_fallback_to_req(
        &mut self,
        sub_id: SubId,
        session: NegSession,
        effects: &mut Vec<Effect>,
    ) {
        effects.push(Effect::NegClose(session.relay.clone(), sub_id.clone()));
        self.attribution.record_send(
            &RelaySessionKey::public(session.relay.clone()),
            &sub_id,
            &session.filter,
            session.absorbed.clone(),
        );
        effects.push(Effect::Wire(WireDelta {
            ops: vec![(
                RelaySessionKey::public(session.relay),
                vec![WireOp::Req(sub_id, session.filter)],
            )],
        }));
    }

    fn refresh_all_handles(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<HandleId> = self.handles.keys().copied().collect();
        self.refresh_handles(ids, effects);
    }

    fn refresh_handles(
        &mut self,
        ids: impl IntoIterator<Item = HandleId>,
        effects: &mut Vec<Effect>,
    ) {
        for id in ids {
            // The resolver also owns internal handles (notably the
            // self-bootstrap discovery query). They participate in graph
            // invalidation but have no app projection state here. Reject
            // them before `refresh_handle` opens any store read.
            if self.handles.contains_key(&id) {
                self.refresh_handle(id, effects);
            }
        }
    }

    fn refresh_all_histories(&mut self, effects: &mut Vec<Effect>) {
        let ids: Vec<_> = self.histories.keys().copied().collect();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
        }
    }

    fn refresh_all_histories_except(
        &mut self,
        except: HistorySessionId,
        effects: &mut Vec<Effect>,
    ) {
        let ids: Vec<_> = self
            .histories
            .keys()
            .copied()
            .filter(|id| *id != except)
            .collect();
        for id in ids {
            self.refresh_history(id, WindowLoad::Idle, effects);
        }
    }

    fn history_batch(
        &mut self,
        id: HistorySessionId,
        deltas: Vec<RowDelta>,
        load: WindowLoad,
    ) -> HistoryBatch {
        let state = self
            .histories
            .get_mut(&id)
            .expect("history batch requires a live session");
        state.load = load;
        let rows = state
            .order
            .iter()
            .filter_map(|(_, event_id)| state.last_rows.get(event_id).cloned())
            .collect();
        HistoryBatch {
            rows,
            deltas,
            evidence: state.last_evidence.clone().unwrap_or_default(),
            load,
        }
    }

    fn refresh_history(
        &mut self,
        id: HistorySessionId,
        load: WindowLoad,
        effects: &mut Vec<Effect>,
    ) -> Option<usize> {
        let (current, evidence) = match self.history_rows_and_evidence_for(id) {
            Ok(value) => value,
            Err(error) => {
                if let Some(state) = self.histories.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(error, effects);
                return None;
            }
        };
        let state = self.histories.get_mut(&id)?;
        let current_rows = current.clone();
        let current_order = current_rows
            .iter()
            .map(|(event_id, row)| (Reverse(row.event.created_at.as_secs()), *event_id))
            .collect();
        let mut deltas = Vec::new();
        for (event_id, row) in current {
            match state.last_rows.get(&event_id) {
                None => deltas.push(RowDelta::Added(row)),
                Some(previous) if previous.sources != row.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in state.last_rows.keys() {
            if !current_rows.contains_key(event_id) {
                deltas.push(RowDelta::Removed(*event_id));
            }
        }
        let changed = !deltas.is_empty()
            || state.last_evidence.as_ref() != Some(&evidence)
            || state.load != load;
        state.last_rows = current_rows;
        state.order = current_order;
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        let len = state.last_rows.len();
        if changed {
            let batch = self.history_batch(id, deltas, load);
            if let Some(state) = self.histories.get(&id) {
                state.sink.on_history(batch.clone());
            }
            effects.push(Effect::EmitHistory(id, batch));
        }
        Some(len)
    }

    fn history_rows_and_evidence_for(
        &self,
        id: HistorySessionId,
    ) -> Result<(BTreeMap<EventId, Row>, AcquisitionEvidence), PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history projection requires a live session");
        let primary = *state
            .handle_ids
            .first()
            .expect("history session always owns its initial resolver handle");
        let root_atoms = self.resolver.root_atoms(primary);
        let subtree_atoms = self.history_subtree_atoms(id);
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
            _ => None,
        };
        let mut by_id = BTreeMap::new();
        for mut atom in root_atoms {
            atom.limit = None;
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match pinned_relays {
                Some(relays) => self.resolver.store().query_newest_observed_by(
                    &filter,
                    relays,
                    state.target_rows,
                )?,
                None => self
                    .resolver
                    .store()
                    .query_newest(&filter, state.target_rows)?,
            };
            #[cfg(test)]
            self.history_rows_examined.set(
                self.history_rows_examined
                    .get()
                    .saturating_add(rows.len() as u64),
            );
            for stored in rows {
                by_id.entry(stored.event.id).or_insert_with(|| Row {
                    event: stored.event,
                    sources: stored.provenance.seen.into_keys().collect(),
                });
            }
        }
        if by_id.len() > state.target_rows {
            let mut ordered: Vec<_> = by_id
                .iter()
                .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
                .collect();
            ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
            let keep: BTreeSet<_> = ordered
                .into_iter()
                .take(state.target_rows)
                .map(|(_, event_id)| event_id)
                .collect();
            by_id.retain(|event_id, _| keep.contains(event_id));
        }
        let auth_status = self.auth_status_map();
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            self.router.plan(),
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        );
        Ok((by_id, evidence))
    }

    /// Every active acquisition atom owned by one coordinated history
    /// partition: initial bounded root, exact unbounded tie seconds, bounded
    /// older ranges, and every interior Derived dependency. Set union keeps
    /// shared atoms deduplicated while preserving distinct scoped windows.
    fn history_subtree_atoms(&self, id: HistorySessionId) -> BTreeSet<ContextualAtom> {
        self.histories
            .get(&id)
            .into_iter()
            .flat_map(|state| state.handle_ids.iter().copied())
            .flat_map(|handle| self.resolver.subtree_atoms(handle))
            .collect()
    }

    fn advance_history_projection(
        &mut self,
        id: HistorySessionId,
        before: nmp_store::EventCursor,
        old_len: usize,
        plan: &RelayPlan,
    ) -> Result<(HistoryBatch, usize), PersistenceError> {
        let state = self
            .histories
            .get(&id)
            .expect("history advance requires a live session");
        let primary = *state
            .handle_ids
            .first()
            .expect("history session always owns its initial resolver handle");
        let root_atoms = self.resolver.root_atoms(primary);
        let subtree_atoms = self.history_subtree_atoms(id);
        let needed = state.target_rows.saturating_sub(state.last_rows.len());
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays),
            _ => None,
        };
        let mut candidates = BTreeMap::<EventId, Row>::new();
        for mut atom in root_atoms {
            atom.limit = None;
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match pinned_relays {
                Some(relays) => self
                    .resolver
                    .store()
                    .query_newest_before_observed_by(&filter, relays, before, needed)?,
                None => self
                    .resolver
                    .store()
                    .query_newest_before(&filter, before, needed)?,
            };
            #[cfg(test)]
            self.history_rows_examined.set(
                self.history_rows_examined
                    .get()
                    .saturating_add(rows.len() as u64),
            );
            for stored in rows {
                candidates.entry(stored.event.id).or_insert_with(|| Row {
                    event: stored.event,
                    sources: stored.provenance.seen.into_keys().collect(),
                });
            }
        }
        let mut ordered: Vec<Row> = candidates.into_values().collect();
        ordered.sort_by(|a, b| {
            nip01_newest_first(
                (a.event.created_at.as_secs(), &a.event.id),
                (b.event.created_at.as_secs(), &b.event.id),
            )
        });
        ordered.truncate(needed);
        let auth_status = self.auth_status_map();
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            plan,
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        );

        let state = self
            .histories
            .get_mut(&id)
            .expect("history remains live during synchronous projection");
        let mut deltas = Vec::with_capacity(ordered.len());
        for row in ordered {
            let event_id = row.event.id;
            state.last_rows.insert(event_id, row.clone());
            state
                .order
                .insert((Reverse(row.event.created_at.as_secs()), event_id));
            deltas.push(RowDelta::Added(row));
        }
        state.last_evidence = Some(evidence);
        state.projection_complete = true;
        let added = state.last_rows.len().saturating_sub(old_len);
        let batch = self.history_batch(id, deltas, WindowLoad::Returned { added });
        Ok((batch, added))
    }

    /// Project one governed store mutation after its crash-atomic commit.
    /// Reactive demand changes may alter router/evidence shape and therefore
    /// keep the broad full-refresh oracle. A stable shape can deliver the
    /// exact durable row facts through #195's fail-safe incremental algebra.
    ///
    /// This is the plain form used by every committed-mutation door that has
    /// no extra non-resolver evidence of its own (`retract`,
    /// `react_to_compensation`, `accept_local`): the resolver's own `delta`
    /// is the ONLY signal for the broad-vs-exact choice.
    fn apply_committed_mutation(
        &mut self,
        committed: CommittedMutationResult,
        effects: &mut Vec<Effect>,
    ) {
        self.apply_committed_mutation_with(committed, false, false, effects);
    }

    /// The one shared refresh-vs-apply decision behind every committed-
    /// mutation door, generalized with two force flags for callers that hold
    /// extra evidence the resolver's `delta` cannot see. Relay ingest is the
    /// only such caller today: an NIP-65 directory winner can change the
    /// capped source plan even when the resolver's own demand shape is
    /// unchanged (`force_recompile`), and a locally-pending write getting
    /// satisfied by a verified relay copy needs every handle re-read even
    /// when neither demand nor directory changed (`force_broad_refresh`,
    /// folded together with `force_recompile` since a directory change also
    /// implies a broad refresh). Both flags default to `false` through
    /// [`Self::apply_committed_mutation`], which reproduces this function's
    /// original (pre-#230) behavior exactly.
    fn apply_committed_mutation_with(
        &mut self,
        committed: CommittedMutationResult,
        force_recompile: bool,
        force_broad_refresh: bool,
        effects: &mut Vec<Effect>,
    ) {
        let CommittedMutationResult {
            delta,
            affected_handles,
            row_changes,
        } = committed;
        let demand_changed = !delta.is_empty();
        let affected: Vec<_> = affected_handles.into_iter().collect();
        let affected_histories: BTreeSet<_> = affected
            .iter()
            .filter_map(|handle| self.history_by_handle.get(handle).copied())
            .collect();
        if demand_changed || force_recompile {
            self.recompile(effects);
        }
        if demand_changed || force_broad_refresh {
            self.refresh_all_handles(effects);
            self.refresh_all_histories(effects);
        } else {
            self.apply_committed_row_changes(affected.iter().copied(), &row_changes, effects);
            for id in affected_histories {
                if !self.try_apply_committed_history_row_changes(id, &row_changes, effects) {
                    self.refresh_history(id, WindowLoad::Idle, effects);
                }
            }
        }
    }

    /// Apply one committed store batch to any stable bounded history window,
    /// including Strict, derived, and multi-root selections. Only touched
    /// rows plus the exact newly exposed lower segment are visited: the
    /// canonical order index identifies eviction/backfill boundaries without
    /// sorting or replaying the retained window.
    fn try_apply_committed_history_row_changes(
        &mut self,
        id: HistorySessionId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let Some(state) = self.histories.get(&id) else {
            return true;
        };
        let Some(primary) = state.handle_ids.first().copied() else {
            return false;
        };
        let root_atoms = self.resolver.root_atoms(primary);
        if state.last_evidence.is_none()
            || !state.projection_complete
            || state.pending_load.is_some()
        {
            return false;
        }
        if root_atoms.is_empty() {
            return state.last_rows.is_empty();
        }
        let filters: Vec<_> = root_atoms
            .into_iter()
            .map(|mut atom| {
                atom.limit = None;
                atom.to_nostr()
            })
            .collect();
        let matches = |event: &nostr::Event| {
            filters
                .iter()
                .any(|filter| filter.match_event(event, MatchEventOptions::new()))
        };
        let pinned_relays = match (
            state.query.live_query().0.cache,
            &state.query.live_query().0.source,
        ) {
            (CacheMode::Strict, SourceAuthority::Pinned(relays)) => Some(relays.clone()),
            _ => None,
        };
        let eligible = |sources: &BTreeSet<RelayUrl>| {
            pinned_relays
                .as_ref()
                .is_none_or(|relays| sources.iter().any(|relay| relays.contains(relay)))
        };
        let target_rows = state.target_rows;
        let original_boundary =
            state
                .order
                .iter()
                .next_back()
                .map(|(Reverse(created_at), event_id)| {
                    nmp_store::EventCursor::new(Timestamp::from(*created_at), *event_id)
                });
        let mut before = BTreeMap::<EventId, Option<Row>>::new();
        let mut visible_removals = 0usize;
        let mut strict_promotions = BTreeMap::<EventId, Row>::new();
        if pinned_relays.is_some() {
            for changed in &changes.provenance_grew {
                if !matches(&changed.event)
                    || !eligible(&changed.observed_relays)
                    || state.last_rows.contains_key(&changed.event.id)
                {
                    continue;
                }
                #[cfg(test)]
                self.history_affected_row_queries
                    .set(self.history_affected_row_queries.get().saturating_add(1));
                let current = match self
                    .resolver
                    .store()
                    .query(&nostr::Filter::new().id(changed.event.id))
                {
                    Ok(mut rows) => rows.pop().map(|stored| Row {
                        event: stored.event,
                        sources: stored.provenance.seen.into_keys().collect(),
                    }),
                    Err(error) => {
                        self.histories
                            .get_mut(&id)
                            .expect("history remained live after affected-row read failure")
                            .projection_complete = false;
                        self.degrade_store(error, effects);
                        return true;
                    }
                };
                strict_promotions.insert(
                    changed.event.id,
                    current.unwrap_or_else(|| Row {
                        event: changed.event.clone(),
                        sources: changed.observed_relays.clone(),
                    }),
                );
            }
        }

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during committed mutation");
            let remember =
                |event_id: EventId,
                 state: &HistoryState,
                 before: &mut BTreeMap<EventId, Option<Row>>| {
                    before
                        .entry(event_id)
                        .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                };

            for event in &changes.removed {
                if !state.last_rows.contains_key(&event.id) {
                    continue;
                }
                remember(event.id, state, &mut before);
                if let Some(row) = state.last_rows.remove(&event.id) {
                    state
                        .order
                        .remove(&(Reverse(row.event.created_at.as_secs()), event.id));
                    visible_removals = visible_removals.saturating_add(1);
                }
            }
            for row in &changes.inserted {
                if !matches(&row.event) || !eligible(&row.observed_relays) {
                    continue;
                }
                let event_id = row.event.id;
                remember(event_id, state, &mut before);
                if let Some(previous) = state.last_rows.remove(&event_id) {
                    state
                        .order
                        .remove(&(Reverse(previous.event.created_at.as_secs()), event_id));
                }
                let remembered = Row {
                    event: row.event.clone(),
                    sources: row.observed_relays.clone(),
                };
                state
                    .order
                    .insert((Reverse(remembered.event.created_at.as_secs()), event_id));
                state.last_rows.insert(event_id, remembered);
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if state.last_rows.contains_key(&row.event.id) {
                    remember(row.event.id, state, &mut before);
                    state
                        .last_rows
                        .get_mut(&row.event.id)
                        .expect("provenance target was checked above")
                        .sources
                        .extend(row.observed_relays.iter().cloned());
                } else if pinned_relays.is_some() && eligible(&row.observed_relays) {
                    // An event already cached from an ineligible relay can
                    // enter a Strict projection when this committed duplicate
                    // is its first eligible observation. Treat that transition
                    // as an affected-row insertion, then let the same bounded
                    // order rebalance decide whether it belongs in top-N.
                    remember(row.event.id, state, &mut before);
                    let projected = strict_promotions
                        .remove(&row.event.id)
                        .expect("eligible Strict promotion was prefetched");
                    state.order.insert((
                        Reverse(projected.event.created_at.as_secs()),
                        projected.event.id,
                    ));
                    state.last_rows.insert(projected.event.id, projected);
                }
            }
        }

        // Any visible removal can expose a better row below the PRE-mutation
        // boundary, even when a simultaneous older insertion/restoration has
        // already brought the working set back to `target_rows`. Reconcile
        // exactly once, merge that bounded tail with every committed affected
        // row above, and only then truncate canonically.
        if visible_removals > 0 {
            let boundary =
                original_boundary.expect("a visible removal implies a prior canonical boundary");
            #[cfg(test)]
            self.history_store_queries
                .set(self.history_store_queries.get().saturating_add(1));
            let queried = match pinned_relays.as_ref() {
                Some(relays) => self.resolver.store().query_newest_before_any_observed_by(
                    &filters,
                    relays,
                    boundary,
                    visible_removals,
                ),
                None => self.resolver.store().query_newest_before_any(
                    &filters,
                    boundary,
                    visible_removals,
                ),
            };
            let rows = match queried {
                Ok(rows) => rows,
                Err(error) => {
                    let state = self
                        .histories
                        .get_mut(&id)
                        .expect("history remained live after failed backfill");
                    for (event_id, prior) in before {
                        if let Some(current) = state.last_rows.remove(&event_id) {
                            state
                                .order
                                .remove(&(Reverse(current.event.created_at.as_secs()), event_id));
                        }
                        if let Some(prior) = prior {
                            state
                                .order
                                .insert((Reverse(prior.event.created_at.as_secs()), event_id));
                            state.last_rows.insert(event_id, prior);
                        }
                    }
                    state.projection_complete = false;
                    self.degrade_store(error, effects);
                    return true;
                }
            };
            #[cfg(test)]
            self.history_rows_examined.set(
                self.history_rows_examined
                    .get()
                    .saturating_add(rows.len() as u64),
            );
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during exact backfill");
            for stored in rows {
                let event_id = stored.event.id;
                if state.last_rows.contains_key(&event_id) {
                    continue;
                }
                before
                    .entry(event_id)
                    .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                let sources: BTreeSet<_> = stored.provenance.seen.into_keys().collect();
                let row = Row {
                    event: stored.event,
                    sources: sources.clone(),
                };
                let remembered = row.clone();
                state
                    .order
                    .insert((Reverse(remembered.event.created_at.as_secs()), event_id));
                state.last_rows.insert(event_id, remembered);
            }
        }

        {
            let state = self
                .histories
                .get_mut(&id)
                .expect("history remained live during canonical truncation");
            let remember =
                |event_id: EventId,
                 state: &HistoryState,
                 before: &mut BTreeMap<EventId, Option<Row>>| {
                    before
                        .entry(event_id)
                        .or_insert_with(|| state.last_rows.get(&event_id).cloned());
                };
            while state.last_rows.len() > target_rows {
                let Some((_, event_id)) = state.order.iter().next_back().copied() else {
                    break;
                };
                remember(event_id, state, &mut before);
                let row = state
                    .last_rows
                    .remove(&event_id)
                    .expect("history order and membership stay identical");
                state
                    .order
                    .remove(&(Reverse(row.event.created_at.as_secs()), event_id));
            }
        }

        let state = self
            .histories
            .get(&id)
            .expect("history remained live after committed rebalance");
        let mut deltas = Vec::new();
        for (event_id, prior) in &before {
            match (prior, state.last_rows.get(event_id)) {
                (None, Some(current)) => deltas.push(RowDelta::Added(current.clone())),
                (Some(_), None) => deltas.push(RowDelta::Removed(*event_id)),
                (Some(prior), Some(current)) if prior.sources != current.sources => {
                    deltas.push(RowDelta::SourcesGrew {
                        id: *event_id,
                        sources: current.sources.clone(),
                    });
                }
                (None, None) | (Some(_), Some(_)) => {}
            }
        }
        if deltas.is_empty() {
            return true;
        }
        let batch = self.history_batch(id, deltas, WindowLoad::Idle);
        if let Some(state) = self.histories.get(&id) {
            state.sink.on_history(batch.clone());
        }
        effects.push(Effect::EmitHistory(id, batch));
        true
    }

    /// Apply a committed writer batch directly to ordinary one-root handle
    /// projections. This is the other half of #177's targeted invalidation:
    /// once the resolver has already proven which handles are affected, a
    /// simple handle should not re-query 60k or 1M prior rows to emit one
    /// exact delta. Complex/multi-root and strict-cache projections keep the
    /// existing full-refresh oracle until their incremental algebra is proven.
    fn apply_committed_row_changes(
        &mut self,
        ids: impl IntoIterator<Item = HandleId>,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) {
        for id in ids {
            if !self.handles.contains_key(&id) {
                continue;
            }
            if !self.try_apply_committed_row_changes(id, changes, effects) {
                self.refresh_handle(id, effects);
            }
        }
    }

    /// Returns `true` when the handle was fully and exactly handled without a
    /// store read (including the no-visible-change case), `false` when the
    /// caller must fall back to `refresh_handle`.
    fn try_apply_committed_row_changes(
        &mut self,
        id: HandleId,
        changes: &CommittedRowChanges,
        effects: &mut Vec<Effect>,
    ) -> bool {
        let root_atoms = self.resolver.root_atoms(id);
        // One currently-resolved root atom is not enough to prove this is
        // an ordinary projection: a Derived/SetOp query can momentarily
        // resolve to one root while still owning interior dependency atoms.
        // Keep those shapes on the full-refresh oracle until their
        // incremental algebra is proven independently.
        if root_atoms.len() != 1 || self.resolver.subtree_atoms(id).len() != 1 {
            return false;
        }
        let atom = root_atoms
            .first()
            .expect("one-root projection has one concrete atom");
        let Some(state) = self.handles.get(&id) else {
            return true;
        };
        if state._handle.cache() == CacheMode::Strict
            || state.last_evidence.is_none()
            || !state.projection_complete
        {
            return false;
        }

        let filter = atom.to_nostr();
        let matches = |event: &nostr::Event| filter.match_event(event, MatchEventOptions::new());
        let row_limit = effective_row_limit(&root_atoms);
        let visible_removal = changes
            .removed
            .iter()
            .any(|event| matches(event) && state.last_rows.contains_key(&event.id));
        // A full top-N window may have older candidates outside remembered
        // state. Removing a visible member therefore needs exactly one
        // bounded oracle read to backfill correctly. Insert-only top-N
        // changes are exact from `old top-N ∪ inserted` and stay read-free.
        if row_limit.is_some_and(|limit| state.last_rows.len() == limit && visible_removal) {
            return false;
        }

        // Unlimited handles are the scale-critical case: mutate remembered
        // selection/provenance state in place and allocate only for the
        // committed delta. Cloning the full BTreeMap here would merely trade
        // a full store replay for O(history) memory/time inside the engine.
        if row_limit.is_none() {
            let state = self
                .handles
                .get_mut(&id)
                .expect("handle remained live during synchronous projection");
            let evidence = state
                .last_evidence
                .clone()
                .expect("direct projection requires prior evidence");
            let mut added = BTreeMap::<EventId, Row>::new();
            let mut sources_grew = BTreeSet::<EventId>::new();
            let mut removed = BTreeSet::<EventId>::new();

            for event in &changes.removed {
                if matches(event) && state.last_rows.remove(&event.id).is_some() {
                    removed.insert(event.id);
                }
            }
            for row in &changes.inserted {
                if !matches(&row.event) {
                    continue;
                }
                let sources = row.observed_relays.clone();
                state.last_rows.insert(
                    row.event.id,
                    RememberedRow {
                        created_at: row.event.created_at.as_secs(),
                        sources: sources.clone(),
                    },
                );
                added.insert(
                    row.event.id,
                    Row {
                        event: row.event.clone(),
                        sources,
                    },
                );
            }
            for row in &changes.provenance_grew {
                if !matches(&row.event) {
                    continue;
                }
                if let Some(remembered) = state.last_rows.get_mut(&row.event.id) {
                    let prior_len = remembered.sources.len();
                    remembered
                        .sources
                        .extend(row.observed_relays.iter().cloned());
                    if remembered.sources.len() != prior_len {
                        sources_grew.insert(row.event.id);
                    }
                }
            }

            let changed_current: BTreeSet<_> =
                added.keys().chain(sources_grew.iter()).copied().collect();
            let mut delta = Vec::with_capacity(changed_current.len() + removed.len());
            for event_id in changed_current {
                if let Some(row) = added.remove(&event_id) {
                    delta.push(RowDelta::Added(row));
                } else {
                    delta.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: state.last_rows[&event_id].sources.clone(),
                    });
                }
            }
            delta.extend(removed.into_iter().map(RowDelta::Removed));
            if delta.is_empty() {
                return true;
            }
            state.sink.on_rows(delta.clone());
            effects.push(Effect::EmitRows(id, delta, evidence));
            return true;
        }

        // Bounded handles remember at most N rows, so cloning their small
        // window is bounded by the caller's explicit limit. This makes
        // insertion/eviction and exact delta ordering straightforward.
        let previous = state.last_rows.clone();
        let mut current = previous.clone();
        let mut added = BTreeMap::<EventId, Row>::new();

        for event in &changes.removed {
            if matches(event) {
                current.remove(&event.id);
            }
        }
        for row in &changes.inserted {
            if !matches(&row.event) {
                continue;
            }
            let sources = row.observed_relays.clone();
            current.insert(
                row.event.id,
                RememberedRow {
                    created_at: row.event.created_at.as_secs(),
                    sources: sources.clone(),
                },
            );
            added.insert(
                row.event.id,
                Row {
                    event: row.event.clone(),
                    sources,
                },
            );
        }
        for row in &changes.provenance_grew {
            if !matches(&row.event) {
                continue;
            }
            if let Some(remembered) = current.get_mut(&row.event.id) {
                remembered
                    .sources
                    .extend(row.observed_relays.iter().cloned());
            }
        }

        let limit = row_limit.expect("unlimited projection returned above");
        if current.len() > limit {
            let mut ordered: Vec<_> = current
                .iter()
                .map(|(event_id, row)| (row.created_at, *event_id))
                .collect();
            ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
            let keep: BTreeSet<_> = ordered
                .into_iter()
                .take(limit)
                .map(|(_, event_id)| event_id)
                .collect();
            current.retain(|event_id, _| keep.contains(event_id));
        }

        if current == previous {
            return true;
        }
        let evidence = state
            .last_evidence
            .clone()
            .expect("direct projection requires prior evidence");
        let mut delta = Vec::new();
        for (event_id, remembered) in &current {
            match previous.get(event_id) {
                None => delta.push(RowDelta::Added(
                    added
                        .remove(event_id)
                        .expect("new direct row came from committed insertion"),
                )),
                Some(last) if last.sources != remembered.sources => {
                    delta.push(RowDelta::SourcesGrew {
                        id: *event_id,
                        sources: remembered.sources.clone(),
                    });
                }
                Some(_) => {}
            }
        }
        for event_id in previous.keys() {
            if !current.contains_key(event_id) {
                delta.push(RowDelta::Removed(*event_id));
            }
        }

        let state = self
            .handles
            .get_mut(&id)
            .expect("handle remained live during synchronous projection");
        state.last_rows = current;
        state.sink.on_rows(delta.clone());
        effects.push(Effect::EmitRows(id, delta, evidence));
        true
    }

    /// Recompute `id`'s current row set + acquisition evidence; emit (and
    /// synchronously deliver to its sink) `Effect::EmitRows` only if either
    /// changed since the last refresh -- and, when something DID change, the
    /// row payload is ALWAYS just the incremental added/sources-grew/removed
    /// delta against `state.last_rows`, never the full current set (see
    /// `RowDelta`'s doc: this is what keeps a long-running subscription's
    /// total delivered row volume ~O(distinct rows) instead of O(rows²)).
    /// Evidence can change with no row change at all (a watermark advancing,
    /// or a source's link status flipping) -- that case still emits,
    /// carrying an EMPTY row delta alongside the new evidence. #105:
    /// per-id provenance growth is detected the SAME way -- a plain value
    /// compare of `state.last_rows`'s remembered source set against this
    /// recompute's -- so a lifecycle-driven recompute of some OTHER
    /// handle's query (`refresh_all_handles`, e.g. on ANY subscribe/
    /// unsubscribe) can never spuriously emit a `SourcesGrew` for a row
    /// whose provenance did not actually change.
    fn refresh_handle(&mut self, id: HandleId, effects: &mut Vec<Effect>) {
        // A read failure while snapshotting this handle's rows (issue #122)
        // degrades to read-only: leave the handle's LAST delivered rows
        // untouched (never fabricate a phantom retraction from a failed
        // read) and surface the degrade on diagnostics instead of panicking.
        let (current, evidence) = match self.rows_and_evidence_for(id) {
            Ok(v) => v,
            Err(e) => {
                if let Some(state) = self.handles.get_mut(&id) {
                    state.projection_complete = false;
                }
                self.degrade_store(e, effects);
                return;
            }
        };
        let Some(state) = self.handles.get_mut(&id) else {
            return;
        };
        let current_rows: BTreeMap<EventId, RememberedRow> = current
            .iter()
            .map(|(id, row)| {
                (
                    *id,
                    RememberedRow {
                        created_at: row.event.created_at.as_secs(),
                        sources: row.sources.clone(),
                    },
                )
            })
            .collect();
        state.projection_complete = true;
        if current_rows == state.last_rows && state.last_evidence.as_ref() == Some(&evidence) {
            return;
        }
        let mut delta: Vec<RowDelta> = Vec::new();
        for (event_id, row) in current {
            match state.last_rows.get(&event_id) {
                None => delta.push(RowDelta::Added(row)),
                Some(last) if last.sources != row.sources => {
                    delta.push(RowDelta::SourcesGrew {
                        id: event_id,
                        sources: row.sources,
                    });
                }
                Some(_) => {}
            }
        }
        for old_id in state.last_rows.keys() {
            if !current_rows.contains_key(old_id) {
                delta.push(RowDelta::Removed(*old_id));
            }
        }
        state.last_rows = current_rows;
        state.last_evidence = Some(evidence.clone());
        state.sink.on_rows(delta.clone());
        effects.push(Effect::EmitRows(id, delta, evidence));
    }

    /// The query's current matching row set (by id) + its
    /// [`AcquisitionEvidence`] -- an internal snapshot `refresh_handle`
    /// diffs against the handle's own remembered `last_rows` to compute the
    /// outgoing delta. This snapshot itself is never handed to a caller/
    /// effect directly.
    ///
    /// #124: when the demand carries a Nostr `limit:N` this projection is the
    /// N MOST RECENT matching rows -- `created_at` DESC, ties broken by event
    /// `id` ASC (bytewise), the NIP-01 canonical newest-first order -- NOT
    /// every cached match. The authoritative cap lives HERE, at the handle
    /// projection, deliberately NOT in `EventStore::query` (which must keep
    /// returning every current match: unlimited Derived-node recompute,
    /// negentropy, and ingest callers rely on its FULL match set. Explicitly
    /// limited Derived nodes use `query_newest` at their own projection seam;
    /// that is a separate NIP-01 event-selection operation, not a mutation of
    /// `query()`'s complete-set contract.
    /// For this projection alone, each root atom may be pre-bounded through
    /// `EventStore::query_newest`; taking N newest from each atom is exact
    /// because a row outside one atom's top N already has N newer witnesses
    /// in that same atom. The final merged/deduped set is still capped ONCE,
    /// per NIP-01 per-subscription `limit` (see [`effective_row_limit`]).
    /// Because `refresh_handle` diffs THIS truncated snapshot against
    /// `last_rows`, the top-N is maintained reactively for free: a newer
    /// match entering the top-N evicts the oldest (Added(new)+Removed(oldest),
    /// never exceeding N), and retracting a top-N member pulls the next-newest
    /// in. `limit: None` is unchanged -- every match, no ordering imposed.
    /// Row truncation NEVER touches `evidence` below (coverage is about what
    /// was acquired, not how many rows are shown -- ledger #17): a limited
    /// query still records no coverage watermark.
    ///
    /// Rows are computed over `root_atoms` alone (delivery
    /// shape unchanged); evidence is computed over `subtree_atoms` (#12: the
    /// query's FULL subtree, interior `Derived` atoms included). Each row
    /// carries its provenance (#105: `StoredEvent::provenance`, already
    /// merged/persisted by `EventStore::insert`'s dedup path) rather than
    /// discarding it -- the mechanism already exists in `nmp-store`; this is
    /// only its honest projection.
    ///
    /// #107: `CacheMode::Strict` applies the pinned cache projection here --
    /// a cached row is returned only when its unioned provenance set
    /// intersects the handle's own pinned relay set (`Row.sources`, #105's
    /// existing field; no new store mechanism). This is read off THIS
    /// handle's own `QueryHandle::cache()`, never the shared graph node's --
    /// two handles sharing the identical (cache-free-deduped) acquisition
    /// key may still disagree on `cache` (Fable's ruling: cache is excluded
    /// from `AcquisitionKey`), so an Agnostic and a Strict handle over the
    /// same pinned selection MUST project different row sets despite
    /// sharing one graph/wire/coverage underneath. The pinned relay set
    /// itself comes from `subtree_atoms`' `source` -- Fable's ruling B
    /// ("uniform per Demand, not subtree") guarantees every atom in a
    /// single handle's subtree carries the SAME declared `SourceAuthority`,
    /// so any one atom's `source` is authoritative for the whole handle.
    /// `CacheMode::Strict` is only meaningful over a `SourceAuthority::
    /// Pinned` selection (the Contract: "pinned cache policy is part of
    /// source identity") -- over any other source there is no pinned relay
    /// set to intersect against, so Strict is a no-op there, identical to
    /// Agnostic.
    fn rows_and_evidence_for(
        &self,
        id: HandleId,
    ) -> Result<(BTreeMap<EventId, Row>, AcquisitionEvidence), PersistenceError> {
        let subtree_atoms = self.resolver.subtree_atoms(id);
        let pinned_relays: Option<&BTreeSet<RelayUrl>> = self
            .handles
            .get(&id)
            .filter(|state| state._handle.cache() == CacheMode::Strict)
            .and_then(|_| {
                subtree_atoms.iter().find_map(|atom| match &atom.source {
                    SourceAuthority::Pinned(relays) => Some(relays),
                    _ => None,
                })
            });

        let root_atoms = self.resolver.root_atoms(id);
        let row_limit = effective_row_limit(&root_atoms);
        let mut by_id: BTreeMap<EventId, Row> = BTreeMap::new();
        for atom in &root_atoms {
            #[cfg(test)]
            self.projection_store_queries
                .set(self.projection_store_queries.get().saturating_add(1));
            let filter = atom.to_nostr();
            let rows = match row_limit {
                Some(limit) => self.resolver.store().query_newest(&filter, limit)?,
                None => self.resolver.store().query(&filter)?,
            };
            for se in rows {
                if let Some(relays) = pinned_relays {
                    if !se
                        .provenance
                        .seen
                        .keys()
                        .any(|relay| relays.contains(relay))
                    {
                        continue;
                    }
                }
                by_id.entry(se.event.id).or_insert_with(|| Row {
                    event: se.event,
                    sources: se.provenance.seen.into_keys().collect(),
                });
            }
        }
        // #124: a demand carrying `limit:N` projects only its N newest rows.
        // Applied authoritatively to the merged/deduped set in NIP-01
        // canonical newest-first order. Each root atom was only pre-bounded
        // above; this final pass preserves the per-subscription (not
        // per-atom) contract. `refresh_handle`'s diff then maintains the
        // top-N reactively. No-op when there is no limit or the set fits.
        if let Some(limit) = row_limit {
            if by_id.len() > limit {
                let mut ordered: Vec<(u64, EventId)> = by_id
                    .iter()
                    .map(|(event_id, row)| (row.event.created_at.as_secs(), *event_id))
                    .collect();
                ordered.sort_by(|a, b| nip01_newest_first((a.0, &a.1), (b.0, &b.1)));
                let keep: BTreeSet<EventId> =
                    ordered.into_iter().take(limit).map(|(_, id)| id).collect();
                by_id.retain(|event_id, _| keep.contains(event_id));
            }
        }
        let auth_status = self.auth_status_map();
        let evidence = evidence::acquisition_evidence(
            &subtree_atoms,
            self.router.plan(),
            self.resolver.store(),
            &self.connected_relays,
            &auth_status,
            &self.ever_connected_relays,
        );
        Ok((by_id, evidence))
    }
}

#[cfg(feature = "bench-instrumentation")]
impl EngineCore<nmp_store::RedbStore> {
    /// Benchmark-only access to the store work counters used by the
    /// million-row scale proofs. Not an application/store API.
    #[doc(hidden)]
    pub fn bench_reset_query_work(&self) {
        self.resolver.store().reset_query_work();
    }

    #[doc(hidden)]
    pub fn bench_query_work(&self) -> (u64, u64, u64) {
        self.resolver.store().query_work()
    }

    /// Drive the production committed-delta path without constructing a
    /// transport frame; the benchmark already owns verified signed events
    /// and explicit relay observations.
    #[doc(hidden)]
    pub fn bench_ingest_observed(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        self.ingest_relay_events(events, &mut effects);
        effects
    }

    /// Exact pre-#195 comparison lane: commit through the same resolver/store
    /// door, then force the old affected-handle full refresh. Restricted to
    /// ordinary benchmark events whose demand/directory shape cannot change.
    #[doc(hidden)]
    pub fn bench_ingest_observed_with_forced_refresh(
        &mut self,
        events: Vec<(SignedEvent, RelayObserved)>,
    ) -> Vec<Effect> {
        let mut effects = Vec::new();
        for (event, observed) in &events {
            // Benchmark observations carry only a URL; they ride the Public
            // session's counter row, the same session the production frame
            // path would attribute an unauthenticated observation to.
            *self
                .events_by_session_kind
                .entry(RelaySessionKey::public(observed.relay.clone()))
                .or_default()
                .entry(event.kind.as_u16())
                .or_insert(0) += 1;
        }
        let ingest = self
            .resolver
            .ingest_observed_detailed(events)
            .expect("benchmark fixture store commit");
        assert!(
            ingest.committed.delta.is_empty(),
            "benchmark shape changed demand"
        );
        assert!(
            ingest.satisfied_intents.is_empty(),
            "benchmark event unexpectedly satisfied a local intent"
        );
        effects.push(Effect::EmitDiagnostics(self.diagnostics_snapshot()));
        self.refresh_handles(ingest.committed.affected_handles, &mut effects);
        effects
    }

    /// Commit a benchmark local write through the real governed
    /// `accept_write`/resolver door, then use the production projection
    /// policy added by #228. Receipt/signing/routing orchestration is outside
    /// the measured mutation seam and deliberately omitted.
    #[doc(hidden)]
    pub fn bench_accept_local(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        let accepted = self
            .resolver
            .accept_local(accept)
            .expect("benchmark local acceptance commit");
        assert!(
            accepted.outcome.journaled_intent_id().is_some(),
            "benchmark local acceptance must be journaled"
        );
        let mut effects = Vec::new();
        self.apply_committed_mutation(accepted.committed, &mut effects);
        effects
    }

    /// Exact pre-#228 comparison for the same local acceptance commit: keep
    /// reactive-demand fallback behavior, but force stable-shape handles
    /// through the former full-refresh projection.
    #[doc(hidden)]
    pub fn bench_accept_local_with_forced_refresh(&mut self, accept: AcceptWrite) -> Vec<Effect> {
        let accepted = self
            .resolver
            .accept_local(accept)
            .expect("benchmark local acceptance commit");
        assert!(
            accepted.outcome.journaled_intent_id().is_some(),
            "benchmark local acceptance must be journaled"
        );
        let CommittedMutationResult {
            delta,
            affected_handles: _,
            row_changes: _,
        } = accepted.committed;
        assert!(delta.is_empty(), "benchmark local write changed demand");
        let mut effects = Vec::new();
        self.recompile(&mut effects);
        self.refresh_all_handles(&mut effects);
        effects
    }

    /// Expire due rows through the production store/retraction/projection
    /// path. The fixture supplies exactly one due row per measured call.
    #[doc(hidden)]
    pub fn bench_expire_due(&mut self, now: Timestamp) -> Vec<Effect> {
        self.bench_expire_due_with_mode(now, false)
    }

    /// Exact pre-#228 expiry comparison: same governed store mutation and
    /// resolver reaction, followed by the former recompile/full refresh.
    #[doc(hidden)]
    pub fn bench_expire_due_with_forced_refresh(&mut self, now: Timestamp) -> Vec<Effect> {
        self.bench_expire_due_with_mode(now, true)
    }

    fn bench_expire_due_with_mode(&mut self, now: Timestamp, force_refresh: bool) -> Vec<Effect> {
        let expired = self
            .resolver
            .store_mut()
            .expire_due(now)
            .expect("benchmark expiry commit");
        assert_eq!(expired.len(), 1, "benchmark owns exactly one due row");
        let removed = expired.into_iter().map(|row| row.event).collect();
        let committed = self
            .resolver
            .retract(removed)
            .expect("benchmark expiry reaction");
        let mut effects = Vec::new();
        if force_refresh {
            let CommittedMutationResult {
                delta,
                affected_handles: _,
                row_changes: _,
            } = committed;
            assert!(delta.is_empty(), "benchmark expiry changed demand");
            self.recompile(&mut effects);
            self.refresh_all_handles(&mut effects);
        } else {
            self.apply_committed_mutation(committed, &mut effects);
        }
        effects
    }
}

/// The demand's effective result cap (NIP-01 `limit:N`) -- the single limit
/// the app's subscription carries, to be applied ONCE to the final merged/
/// deduped row set the handle projects, never per-atom (#124). A demand fans
/// out into many `root_atoms` only via the cartesian product of its bound
/// fields' resolved elements (`Graph::compute_atoms`), and every one of those
/// atoms is a clone of the SAME base filter -- so they all carry the
/// IDENTICAL `limit`. Reducing with `max` over that invariantly-uniform set
/// is therefore just a defensive fold that yields exactly that shared value;
/// `None` iff the demand carried no limit at all (the whole set is projected,
/// unordered). For a union/multi-atom demand this is the deliberate choice:
/// NIP-01's `limit` is a property of the subscription, so the app sees the N
/// newest rows across the WHOLE union, not N per operand.
fn effective_row_limit(root_atoms: &BTreeSet<ConcreteFilter>) -> Option<usize> {
    // The uniform-limit invariant this fold rests on: every fanned root atom
    // is a clone of the same base filter, so they all carry the IDENTICAL
    // `limit`. `max` therefore returns exactly that shared value. If a future
    // graph change ever broke that assumption, `max` would silently
    // over-return (project the largest atom's N while smaller-N atoms wanted
    // fewer) -- so pin it here: a mixed-limit root set trips in tests rather
    // than degrading semantics in release (debug-only, zero release cost).
    debug_assert!(
        root_atoms
            .iter()
            .map(|atom| atom.limit)
            .collect::<BTreeSet<_>>()
            .len()
            <= 1,
        "root_atoms must share a single limit (NIP-01 limit is per-subscription); \
         got a mixed-limit set: {root_atoms:?}",
    );
    root_atoms.iter().filter_map(|atom| atom.limit).max()
}

/// The NIP-01 canonical newest-first total order used to pick the N most
/// recent rows for a `limit:N` demand (#124): `created_at` DESC, ties broken
/// by event `id` ASC compared bytewise -- the same deterministic order a
/// relay applies when it answers a limited REQ with "the `limit` most recent
/// events". Each argument is a `(created_at_secs, &id)` pair.
fn nip01_newest_first(a: (u64, &EventId), b: (u64, &EventId)) -> std::cmp::Ordering {
    b.0.cmp(&a.0)
        .then_with(|| a.1.as_bytes().cmp(b.1.as_bytes()))
}

/// Parse NIP-65 `r` tags off a kind:10002 event into its WRITE relay set
/// (lane `Nip65Write`): an absent marker or an explicit `"write"` marker is
/// a write relay; an explicit `"read"` marker is excluded. Mirrors
/// `nmp-demo`'s former one-shot bootstrap parse exactly (the same NIP-65
/// semantics), now run reactively per event instead of once up front.
fn parse_nip65_write_relays(event: &nostr::Event) -> Vec<LanedRelay> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) != Some("r") {
                return None;
            }
            let url = RelayUrl::parse(s.get(1)?).ok()?;
            match s.get(2).map(String::as_str) {
                Some("read") => None,
                _ => Some(LanedRelay::new(url, Lane::Nip65Write)),
            }
        })
        .collect()
}

/// Parse NIP-65 `r` tags off a kind:10002 event into its READ relay set
/// (lane `Nip65Read`): the mirror of `parse_nip65_write_relays` -- an
/// absent marker or an explicit `"read"` marker is a read relay; an
/// explicit `"write"` marker is excluded (`routing-and-ownership.md` §2.4 --
/// an unmarked `r` tag counts as BOTH read and write, per NIP-65).
fn parse_nip65_read_relays(event: &nostr::Event) -> Vec<LanedRelay> {
    event
        .tags
        .iter()
        .filter_map(|t| {
            let s = t.as_slice();
            if s.first().map(String::as_str) != Some("r") {
                return None;
            }
            let url = RelayUrl::parse(s.get(1)?).ok()?;
            match s.get(2).map(String::as_str) {
                Some("write") => None,
                _ => Some(LanedRelay::new(url, Lane::Nip65Read)),
            }
        })
        .collect()
}

#[cfg(test)]
mod receipt_allocator_tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{Keys, Kind};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for Sink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn rejected_intent(keys: &Keys, created_at: u64) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Unsigned(UnsignedEvent::new(
                keys.public_key(),
                Timestamp::from(created_at),
                Kind::TextNote,
                vec![],
                "no active account",
            )),
            durability: Durability::Durable,
            routing: WriteRouting::AuthorOutbox,
            identity_override: None,
        }
    }

    #[test]
    fn stale_replaceable_edit_surfaces_a_typed_conflict_before_acceptance() {
        use nmp_store::RelayObserved;
        use nostr::EventBuilder;

        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://source.example").unwrap();
        let base = EventBuilder::new(Kind::ContactList, "base")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let concurrent = EventBuilder::new(Kind::ContactList, "concurrent")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                base.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(10u64)),
            )
            .unwrap();
        store
            .insert(
                concurrent.clone(),
                RelayObserved::new(relay, Timestamp::from(20u64)),
            )
            .unwrap();

        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let sink = Sink::default();
        let effects = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::UnsignedReplaceableEdit {
                    unsigned: UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(30u64),
                        Kind::ContactList,
                        vec![],
                        "my edit",
                    ),
                    expected_base: Some(base.id),
                },
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(sink.clone()),
        ));

        let expected = WriteStatus::ReplaceableConflict {
            expected: Some(base.id),
            actual: Some(concurrent.id),
        };
        assert_eq!(
            sink.0.lock().unwrap().as_slice(),
            std::slice::from_ref(&expected)
        );
        assert!(effects
            .iter()
            .any(|effect| matches!(effect, Effect::EmitReceipt(_, status) if *status == expected)));
        assert!(core.pending.is_empty());
        assert!(core.resolver.store().recover_outbox().is_empty());
    }

    #[test]
    fn last_upper_half_id_is_issued_once_then_exhaustion_is_stable_and_typed() {
        const FIRST_UNACCEPTED_ID: u64 = 1u64 << 63;
        let keys = Keys::generate();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_unaccepted_receipt_for_test(Some(FIRST_UNACCEPTED_ID));

        let last_sink = Sink::default();
        let last = core.handle(EngineMsg::Publish(
            rejected_intent(&keys, 1),
            Box::new(last_sink.clone()),
        ));
        assert!(last.iter().any(|effect| {
            matches!(
                effect,
                Effect::EmitReceipt(ReceiptId(id), WriteStatus::Failed(_))
                    if *id == FIRST_UNACCEPTED_ID
            )
        }));
        assert!(matches!(
            last_sink.0.lock().unwrap().as_slice(),
            [WriteStatus::Failed(_)]
        ));

        for created_at in [2, 3] {
            let exhausted_sink = Sink::default();
            let exhausted = core.handle(EngineMsg::Publish(
                rejected_intent(&keys, created_at),
                Box::new(exhausted_sink.clone()),
            ));
            assert!(matches!(
                exhausted.as_slice(),
                [Effect::PublishFailed(
                    PublishError::ReceiptCorrelationIdExhausted
                )]
            ));
            assert!(exhausted_sink.0.lock().unwrap().is_empty());
            assert!(!exhausted
                .iter()
                .any(|effect| matches!(effect, Effect::EmitReceipt(..))));
        }

        assert_eq!(FIRST_UNACCEPTED_ID - 1, u64::MAX >> 1);
        assert!(core.pending.is_empty());
        assert!(core.resolver.store().recover_outbox().is_empty());
    }

    #[test]
    fn last_attempt_correlation_is_issued_once_then_exhaustion_is_stable_and_typed() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        core.set_next_attempt_correlation_for_test(Some(u64::MAX));

        assert_eq!(
            core.alloc_attempt_correlation(),
            Ok(AttemptCorrelation(u64::MAX))
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted)
        );
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "exhaustion remains stable: no wrap, reuse, or fabricated id"
        );
    }

    #[test]
    fn attempt_correlation_exhaustion_precedes_lane_and_pending_mutation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://correlation-exhausted.example").unwrap();
        let directory =
            FixtureDirectory::new().with_write(keys.public_key().to_hex(), [relay.clone()]);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(directory), 10);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let accepted = core.handle(EngineMsg::Publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(93u64),
                    Kind::TextNote,
                    vec![],
                    "correlation boundary",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::AuthorOutbox,
                identity_override: None,
            },
            Box::new(Sink::default()),
        ));
        let (receipt, generation, unsigned) = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::RequestSign(receipt, generation, unsigned) => {
                    Some((*receipt, *generation, unsigned.clone()))
                }
                _ => None,
            })
            .expect("accepted unsigned intent requests signing");
        let intent = core.pending[&receipt].intent_id.unwrap();
        core.set_next_attempt_correlation_for_test(None);

        let effects = core.handle(EngineMsg::SignerCompleted(
            receipt,
            generation,
            Ok(unsigned.sign_with_keys(&keys).unwrap()),
        ));

        assert!(!effects
            .iter()
            .any(|effect| matches!(effect, Effect::PublishEvent(..))));
        assert!(core.attempt_correlations.is_empty());
        assert!(core.pending[&receipt].pending_relays.is_empty());
        assert!(core.pending[&receipt].attempt_ordinals.is_empty());
        assert!(core
            .resolver
            .store()
            .recover_attempts(intent)
            .unwrap()
            .is_empty());
        assert_eq!(
            core.alloc_attempt_correlation(),
            Err(AttemptCorrelationExhausted),
            "the failed call must not revive or wrap the namespace"
        );
    }
}

#[cfg(test)]
mod nip65_read_write_split_tests {
    //! Unit A's NIP-65 read/write parse split (`routing-and-ownership.md`
    //! §2.4) -- private free functions, so tested directly in-module rather
    //! than via the heavier `tests/self_bootstrap_outbox.rs`-style engine
    //! harness (which already covers `parse_nip65_write_relays` end-to-end
    //! via `relay_list_parse_excludes_explicit_read_only_relays`).

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::nips::nip65::RelayMetadata;
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    use super::*;

    fn relay_list_event(author: &Keys, tags: Vec<Tag>) -> nostr::Event {
        EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly")
    }

    #[test]
    fn nip65_unmarked_relay_is_both_read_and_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://both.example.com").unwrap();
        let event = relay_list_event(&author, vec![Tag::relay_metadata(r.clone(), None)]);

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r.clone(), Lane::Nip65Write)],
            "an unmarked r tag must count as a write relay"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an unmarked r tag must ALSO count as a read relay (NIP-65: unmarked = both)"
        );
    }

    #[test]
    fn nip65_write_marked_excluded_from_read() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Write))],
        );

        assert_eq!(
            parse_nip65_write_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Write)],
            "an explicit write-marked relay must still be a write relay"
        );
        assert!(
            parse_nip65_read_relays(&event).is_empty(),
            "an explicit write-marked relay must be excluded from the read set"
        );
    }

    #[test]
    fn nip65_read_marked_excluded_from_write() {
        let author = Keys::generate();
        let r = RelayUrl::parse("wss://read-only.example.com").unwrap();
        let event = relay_list_event(
            &author,
            vec![Tag::relay_metadata(r.clone(), Some(RelayMetadata::Read))],
        );

        assert!(
            parse_nip65_write_relays(&event).is_empty(),
            "an explicit read-marked relay must be excluded from the write set"
        );
        assert_eq!(
            parse_nip65_read_relays(&event),
            vec![LanedRelay::new(r, Lane::Nip65Read)],
            "an explicit read-marked relay must still be a read relay"
        );
    }

    /// `ingest_relay_list_winner` stores BOTH sets from the ONE kind:10002
    /// winner in a single pass (`routing-and-ownership.md` §2.4) -- proven
    /// through the real `EngineCore::on_relay_frame` path (not a bypassed
    /// direct directory poke), against a relay list mixing an unmarked
    /// (both), an explicit write-only, and an explicit read-only relay.
    #[test]
    fn live_directory_stores_read_and_write_from_one_winner() {
        let author = Keys::generate();
        let relay_url = RelayUrl::parse("wss://relay.example.com").unwrap();
        let both = RelayUrl::parse("wss://both.example.com").unwrap();
        let write_only = RelayUrl::parse("wss://write-only.example.com").unwrap();
        let read_only = RelayUrl::parse("wss://read-only.example.com").unwrap();

        let dir = LiveDirectory::builder().build();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(dir), 10);

        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            RelaySessionKey::public(relay_url.clone()),
        ));

        let event = relay_list_event(
            &author,
            vec![
                Tag::relay_metadata(both.clone(), None),
                Tag::relay_metadata(write_only.clone(), Some(RelayMetadata::Write)),
                Tag::relay_metadata(read_only.clone(), Some(RelayMetadata::Read)),
            ],
        );
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: 0,
                generation: 1,
            },
            RelaySessionKey::public(relay_url),
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
        ));

        let author_hex = author.public_key().to_hex();
        let write_relays: BTreeSet<RelayUrl> = core
            .directory
            .write_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        let read_relays: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();

        assert_eq!(
            write_relays,
            BTreeSet::from([both.clone(), write_only.clone()]),
            "write set must be {{unmarked, write-marked}}, excluding read-marked"
        );
        assert_eq!(
            read_relays,
            BTreeSet::from([both, read_only]),
            "read set must be {{unmarked, read-marked}}, excluding write-marked"
        );
    }
}

#[cfg(test)]
mod relay_admission_tests {
    //! Issue #121 falsifiers for the provenance-aware discovered-relay
    //! admission gate. All exercise the REAL `EngineCore::on_relay_frame`
    //! ingest path (a validly-signed kind:10002 delivered over the wire),
    //! never a bypassed direct directory poke -- the whole point is that a
    //! *validly signed but hostile* relay list is what we must reject.
    //!
    //! "Never reaches `ensure_open`" is proven structurally: a rejected relay
    //! is absent from `directory.write_relays`/`read_relays`, so the router
    //! never builds a candidate for it, so no `Effect` ever names it, so
    //! `runtime::dispatch_effect` never calls `pool.ensure_open` on it. Each
    //! test pins that absence at the directory, the choke point where a
    //! discovered relay would otherwise become a routable lane.

    use nmp_router::LiveDirectory;
    use nmp_store::MemoryStore;
    use nmp_transport::RelayFrame;
    use nostr::{EventBuilder, Keys, Kind, RelayMessage, SubscriptionId, Tag, Tags};

    // `RelayDirectory` (the trait whose `write_relays`/`read_relays` these
    // tests call) is already in scope via `use super::*` — importing it again
    // here is a redundant-import warning under `-D warnings`.
    use super::*;

    const SLOT: u32 = 0;
    const GEN: u64 = 1;

    fn relay(url: &str) -> RelayUrl {
        RelayUrl::parse(url).expect("valid test relay url")
    }

    /// Drive a signed kind:10002 (declaring every `url` as an unmarked
    /// read+write relay) through the engine's real ingest path.
    fn ingest_relay_list(core: &mut EngineCore<MemoryStore>, author: &Keys, urls: &[&RelayUrl]) {
        // A connected relay is the one the discovery frame arrives on.
        core.handle(EngineMsg::RelayConnected(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            RelaySessionKey::public(relay("wss://indexer.example.com")),
        ));
        let tags: Vec<Tag> = urls
            .iter()
            .map(|u| Tag::relay_metadata((*u).clone(), None))
            .collect();
        let event = EventBuilder::new(Kind::RelayList, "")
            .tags(Tags::from_list(tags))
            .sign_with_keys(author)
            .expect("test fixture event must sign cleanly");
        core.handle(EngineMsg::RelayFrame(
            TransportRelayHandle {
                slot: SLOT,
                generation: GEN,
            },
            RelaySessionKey::public(relay("wss://indexer.example.com")),
            RelayFrame::from(RelayMessage::event(SubscriptionId::new("s"), event)),
        ));
    }

    fn admitted_writes(core: &EngineCore<MemoryStore>, author: &Keys) -> BTreeSet<RelayUrl> {
        core.directory
            .write_relays(&author.public_key().to_hex())
            .into_iter()
            .map(|lr| lr.url)
            .collect()
    }

    /// The headline falsifier: a validly-signed, network-DISCOVERED kind:10002
    /// listing a loopback, an RFC-1918, and a `.onion` relay alongside one
    /// public relay must admit ONLY the public relay. The three hostile
    /// relays never become lanes (so never reach `ensure_open`), and the
    /// diagnostic rejection counter records exactly them -- for BOTH the read
    /// and write parse of the one event (2.4's dual parse), i.e. 3 hosts ×
    /// 2 lanes = 6 rejections.
    #[test]
    fn discovered_private_and_onion_relays_are_rejected_and_counted() {
        let author = Keys::generate();
        let public = relay("wss://relay.example.com");
        let loopback = relay("ws://127.0.0.1:7777");
        let rfc1918 = relay("ws://10.0.0.5");
        let onion = relay("ws://expyuzz4wqqyqhjn.onion");

        // Secure default: empty allowlist.
        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&public, &loopback, &rfc1918, &onion]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([public.clone()]),
            "only the public relay may become a discovered write lane"
        );
        let author_hex = author.public_key().to_hex();
        let admitted_reads: BTreeSet<RelayUrl> = core
            .directory
            .read_relays(&author_hex)
            .into_iter()
            .map(|lr| lr.url)
            .collect();
        assert_eq!(
            admitted_reads,
            BTreeSet::from([public]),
            "the read lane is gated identically -- no hostile host leaks in via read"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 6,
            "3 hostile hosts rejected on each of the write AND read parse of the one event"
        );
        assert_eq!(
            core.diagnostics_snapshot()
                .discovered_private_relays_rejected,
            6,
            "the rejection count must be visible in diagnostics (issue #121)"
        );
    }

    /// A user who EXPLICITLY opts a local host in re-admits a DISCOVERED relay
    /// on exactly that host -- provenance the transport layer lacks, which is
    /// why this decision lives in the engine. A different local host stays
    /// rejected.
    #[test]
    fn user_configured_local_host_admits_that_discovered_relay() {
        let author = Keys::generate();
        let opted_in = relay("ws://127.0.0.1:7777");
        let other_local = relay("ws://10.0.0.5");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        )
        .with_relay_admission(RelayAdmissionPolicy::new(["127.0.0.1".to_string()]));
        ingest_relay_list(&mut core, &author, &[&opted_in, &other_local]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([opted_in]),
            "the opted-in local host is admitted; a different local host is not"
        );
        assert_eq!(
            core.discovered_private_relays_rejected, 2,
            "only the non-opted-in local host is rejected -- once per lane parse"
        );
    }

    /// The "HOST, never path" falsifier at the engine layer: a real per-user
    /// relay served at a URL PATH is public and must be admitted from
    /// discovery, untouched by the SSRF gate.
    #[test]
    fn discovered_public_host_at_a_path_is_admitted() {
        let author = Keys::generate();
        let per_user = relay("wss://nostr.wine/npub1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");

        let mut core = EngineCore::new(
            MemoryStore::new(),
            Box::new(LiveDirectory::builder().build()),
            10,
        );
        ingest_relay_list(&mut core, &author, &[&per_user]);

        assert_eq!(
            admitted_writes(&core, &author),
            BTreeSet::from([per_user]),
            "a public host with a per-user path must pass admission -- the path is not a host"
        );
        assert_eq!(core.discovered_private_relays_rejected, 0);
    }
}

#[cfg(test)]
mod relay_health_tests {
    use super::*;
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;

    #[test]
    fn verifier_outage_reaches_engine_diagnostics_without_false_misbehavior() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 10);
        let handle = TransportRelayHandle {
            slot: 7,
            generation: 1,
        };
        let session = RelaySessionKey::public(RelayUrl::parse("wss://health.example.com").unwrap());
        let health = RelayHealth {
            last_error: Some("signature verification worker unavailable".to_string()),
            invalid_signature_count: 0,
            ..RelayHealth::default()
        };

        // Health for a slot never seen connected is ignored (#8): it can
        // name no verified (handle, session) pair to attribute itself to.
        assert!(core
            .handle(EngineMsg::RelayHealth(
                handle,
                session.clone(),
                health.clone(),
            ))
            .is_empty());
        assert!(core.diagnostics_snapshot().transport_degraded.is_none());

        core.handle(EngineMsg::RelayConnected(handle, session.clone()));
        let effects = core.handle(EngineMsg::RelayHealth(handle, session, health));
        assert!(effects.iter().any(|effect| {
            matches!(effect, Effect::EmitDiagnostics(snapshot)
                if snapshot.transport_degraded.as_deref()
                    == Some("signature verification worker unavailable"))
        }));
        assert_eq!(
            core.diagnostics_snapshot().transport_degraded.as_deref(),
            Some("signature verification worker unavailable")
        );
    }
}

#[cfg(test)]
mod history_load_failure_tests;

#[cfg(test)]
mod affected_handle_invalidation_tests {
    use std::sync::{Arc, Mutex};

    use nmp_grammar::IndexedTagName;
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    const HANDLE_COUNT: usize = 64;
    const ROWS_PER_HANDLE: usize = 4;

    #[derive(Clone, Default)]
    struct CapturingSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

    impl RowSink for CapturingSink {
        fn on_rows(&self, rows: Vec<RowDelta>) {
            self.0.lock().unwrap().push(rows);
        }
    }

    #[derive(Clone, Default)]
    struct CapturingReceiptSink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for CapturingReceiptSink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn room_event(keys: &Keys, room: usize, ordinal: usize, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::from(9u16), format!("room-{room}-event-{ordinal}"))
            .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn room_query_for_kind(room: usize, kind: u16, limit: usize) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([kind])),
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from([format!("room-{room}")])),
            )]),
            limit: Some(limit),
            ..Filter::default()
        })
    }

    fn room_query(room: usize) -> LiveQuery {
        room_query_for_kind(room, 9, 200)
    }

    fn unlimited_room_query(room: usize) -> LiveQuery {
        LiveQuery::from_filter(Filter {
            tags: BTreeMap::from([(
                IndexedTagName::new('h').unwrap(),
                Binding::Literal(BTreeSet::from([format!("room-{room}")])),
            )]),
            ..Filter::default()
        })
    }

    fn pinned_signed_intent(event: SignedEvent, relay: &RelayUrl) -> WriteIntent {
        WriteIntent {
            payload: WritePayload::Signed(event),
            durability: Durability::Durable,
            routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay.clone())),
            identity_override: None,
        }
    }

    fn subscribed_handle(effects: &[Effect]) -> HandleId {
        effects
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(id, _, _) => Some(*id),
                _ => None,
            })
            .expect("subscribe emits the initial row/evidence snapshot")
    }

    fn assert_remembered_rows_match_oracle(core: &EngineCore<MemoryStore>, id: HandleId) {
        let (oracle, _) = core.rows_and_evidence_for(id).unwrap();
        let oracle: BTreeMap<_, _> = oracle
            .into_iter()
            .map(|(event_id, row)| {
                (
                    event_id,
                    RememberedRow {
                        created_at: row.event.created_at.as_secs(),
                        sources: row.sources,
                    },
                )
            })
            .collect();
        assert_eq!(core.handles[&id].last_rows, oracle);
    }

    #[test]
    fn local_signed_acceptance_updates_unlimited_handle_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-delta.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);

        let initial = room_event(&keys, 7, 0, 10);
        core.resolver
            .store_mut()
            .insert(
                initial,
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(7),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let arriving = room_event(&keys, 7, 1, 12);
        let effects = core.on_publish(
            pinned_signed_intent(arriving.clone(), &relay),
            Box::new(CapturingReceiptSink::default()),
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Added(row)]
                    if row.event.id == arriving.id)
            }
            _ => false,
        }));
        let batches = rows.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0].as_slice(),
            [RowDelta::Added(row)]
                if row.event.id == arriving.id && row.sources.is_empty()
        ));
        drop(batches);
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn demand_changing_local_acceptance_keeps_the_full_refresh_oracle() {
        let author = Keys::generate();
        let followed = Keys::generate();
        let relay = RelayUrl::parse("wss://local-demand-change.example").unwrap();
        let followed_post = nmp_resolver::testkit::kind1(&followed, "already cached", 10);
        let mut store = MemoryStore::new();
        store
            .insert(
                followed_post.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.handle(EngineMsg::SetActivePubkey(Some(author.public_key())));

        let follows_query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([1u16])),
            authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
                inner: nmp_grammar::Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: nmp_grammar::Selector::Tag("p".to_owned()),
            }))),
            ..Filter::default()
        });
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(follows_query, Box::new(rows.clone())));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let contact_list = nmp_resolver::testkit::kind3(&author, &[followed.public_key()], 20);
        let effects = core.on_publish(
            pinned_signed_intent(contact_list, &relay),
            Box::new(CapturingReceiptSink::default()),
        );

        assert_eq!(core.router_compiles.get(), 1);
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => deltas
                .iter()
                .any(|delta| matches!(delta, RowDelta::Added(row)
                    if row.event.id == followed_post.id)),
            _ => false,
        }));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_compensation_removes_pending_row_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-compensation.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        core.active_pubkey = Some(keys.public_key());
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(9),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();

        let unsigned = UnsignedEvent::new(
            keys.public_key(),
            Timestamp::from(21u64),
            Kind::from(9u16),
            vec![Tag::parse(["h".to_owned(), "room-9".to_owned()]).unwrap()],
            "pending local row",
        );
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::Unsigned(unsigned),
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay)),
                identity_override: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("local acceptance emits its receipt");
        let pending_id = rows.0.lock().unwrap()[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("pending row was projected");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);

        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let cancelled = core.on_cancel_write(receipt);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(cancelled.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Removed(event_id)]
                    if *event_id == pending_id)
            }
            _ => false,
        }));
        let batches = rows.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0].as_slice(),
            [RowDelta::Removed(event_id)] if *event_id == pending_id
        ));
        drop(batches);
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_top_n_compensation_uses_one_bounded_backfill_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-top-n.example").unwrap();
        let oldest = room_event(&keys, 10, 0, 10);
        let retained = room_event(&keys, 10, 1, 20);
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                [oldest.clone(), retained]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(21u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 21);
        core.active_pubkey = Some(keys.public_key());
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            room_query_for_kind(10, 9, 2),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(30u64),
                    Kind::from(9u16),
                    vec![Tag::parse(["h".to_owned(), "room-10".to_owned()]).unwrap()],
                    "newest pending",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay.clone())),
                identity_override: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("local acceptance emits its receipt");
        let pending_id = rows.0.lock().unwrap()[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("new pending row is visible");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(rows.0.lock().unwrap()[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == oldest.id)));
        assert_remembered_rows_match_oracle(&core, handle);

        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        core.on_cancel_write(receipt);

        assert_eq!(core.projection_store_queries.get(), 1);
        assert_eq!(core.router_compiles.get(), 0);
        let batches = rows.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending_id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == oldest.id)));
        drop(batches);
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_replaceable_compensation_restores_predecessor_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-replaceable.example").unwrap();
        let predecessor = EventBuilder::new(Kind::ContactList, "old")
            .tag(Tag::public_key(Keys::generate().public_key()))
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                predecessor.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.active_pubkey = Some(keys.public_key());
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            LiveQuery::from_filter(Filter::default()),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::UnsignedReplaceableEdit {
                    unsigned: UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(20u64),
                        Kind::ContactList,
                        vec![Tag::public_key(Keys::generate().public_key())],
                        "new",
                    ),
                    expected_base: Some(predecessor.id),
                },
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay)),
                identity_override: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("replaceable acceptance emits its receipt");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let accepted_batches = rows.0.lock().unwrap();
        assert_eq!(accepted_batches.len(), 1);
        let pending_id = accepted_batches[0]
            .iter()
            .find_map(|delta| match delta {
                RowDelta::Added(row) => Some(row.event.id),
                _ => None,
            })
            .expect("new pending winner was added");
        assert!(accepted_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == predecessor.id)));
        drop(accepted_batches);
        assert_remembered_rows_match_oracle(&core, handle);

        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        core.on_cancel_write(receipt);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        let cancelled_batches = rows.0.lock().unwrap();
        assert_eq!(cancelled_batches.len(), 1);
        assert!(cancelled_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == predecessor.id)));
        assert!(cancelled_batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending_id)));
        drop(cancelled_batches);
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn local_kind5_compensation_reveals_target_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-kind5.example").unwrap();
        let target = room_event(&keys, 13, 0, 10);
        let mut store = MemoryStore::new();
        store
            .insert(
                target.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.active_pubkey = Some(keys.public_key());
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(13),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::Unsigned(UnsignedEvent::new(
                    keys.public_key(),
                    Timestamp::from(20u64),
                    Kind::EventDeletion,
                    vec![Tag::event(target.id)],
                    "",
                )),
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay)),
                identity_override: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("kind5 acceptance emits its receipt");
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(matches!(
            rows.0.lock().unwrap().as_slice(),
            [batch]
                if matches!(batch.as_slice(), [RowDelta::Removed(id)] if *id == target.id)
        ));
        assert_remembered_rows_match_oracle(&core, handle);

        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        core.on_cancel_write(receipt);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(matches!(
            rows.0.lock().unwrap().as_slice(),
            [batch]
                if matches!(batch.as_slice(), [RowDelta::Added(row)] if row.event.id == target.id)
        ));
        assert_remembered_rows_match_oracle(&core, handle);
    }

    #[test]
    fn nip40_expiry_removes_unlimited_row_without_projection_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-expiry.example").unwrap();
        let expiring = EventBuilder::new(Kind::from(9u16), "expires")
            .tag(Tag::parse(["h".to_owned(), "room-11".to_owned()]).unwrap())
            .tag(Tag::expiration(Timestamp::from(100u64)))
            .custom_created_at(Timestamp::from(50u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert(
                expiring.clone(),
                RelayObserved::new(relay, Timestamp::from(51u64)),
            )
            .unwrap();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 51);
        let rows = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(11),
            Box::new(rows.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        rows.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);

        let effects = core.handle(EngineMsg::Tick(Timestamp::from(100u64)));

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(effects.iter().any(|effect| match effect {
            Effect::EmitRows(id, deltas, _) if *id == handle => {
                matches!(deltas.as_slice(), [RowDelta::Removed(event_id)]
                    if *event_id == expiring.id)
            }
            _ => false,
        }));
        let batches = rows.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(matches!(
            batches[0].as_slice(),
            [RowDelta::Removed(event_id)] if *event_id == expiring.id
        ));
        drop(batches);
        assert_remembered_rows_match_oracle(&core, handle);
    }

    fn apply_local_differential_accept(
        core: &mut EngineCore<MemoryStore>,
        event: SignedEvent,
        accepted_at: u64,
        direct: bool,
    ) -> (IntentId, SignedEvent) {
        let accepted = core
            .resolver
            .accept_local(nmp_resolver::testkit::accept_write_of(event, accepted_at))
            .unwrap();
        let (intent_id, pending) = match &accepted.outcome {
            AcceptOutcome::Inserted { intent_id, row, .. }
            | AcceptOutcome::Superseded { intent_id, row, .. }
            | AcceptOutcome::Kind5Processed { intent_id, row, .. } => {
                (*intent_id, row.event.clone())
            }
            other => panic!("differential mutation must commit a pending row, got {other:?}"),
        };
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(accepted.committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_handles(&mut effects);
        }
        (intent_id, pending)
    }

    fn apply_local_differential_compensation(
        core: &mut EngineCore<MemoryStore>,
        intent_id: IntentId,
        pending: SignedEvent,
        direct: bool,
    ) {
        let outcome = core
            .resolver
            .store_mut()
            .compensate_write(intent_id)
            .unwrap();
        let committed = core
            .resolver
            .react_to_compensation(pending, &outcome)
            .unwrap();
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_handles(&mut effects);
        }
    }

    fn apply_local_differential_expiry(
        core: &mut EngineCore<MemoryStore>,
        now: Timestamp,
        direct: bool,
    ) {
        let expired = core.resolver.store_mut().expire_due(now).unwrap();
        let removed = expired.into_iter().map(|row| row.event).collect();
        let committed = core.resolver.retract(removed).unwrap();
        let mut effects = Vec::new();
        if direct {
            core.apply_committed_mutation(committed, &mut effects);
        } else {
            core.recompile(&mut effects);
            core.refresh_all_handles(&mut effects);
        }
    }

    #[test]
    fn mixed_local_accept_compensate_and_expiry_match_forced_full_refresh() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://local-differential.example").unwrap();
        let predecessor = EventBuilder::new(Kind::ContactList, "old")
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&keys)
            .unwrap();
        let target = room_event(&keys, 31, 0, 11);
        let expiring = EventBuilder::new(Kind::TextNote, "expires")
            .tag(Tag::expiration(Timestamp::from(100u64)))
            .custom_created_at(Timestamp::from(12u64))
            .sign_with_keys(&keys)
            .unwrap();
        let seed = [predecessor.clone(), target.clone(), expiring.clone()];

        let make_core = || {
            let mut store = MemoryStore::new();
            store
                .insert_batch(
                    seed.iter()
                        .cloned()
                        .map(|event| {
                            (
                                event,
                                RelayObserved::new(relay.clone(), Timestamp::from(13u64)),
                            )
                        })
                        .collect(),
                )
                .unwrap();
            let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 13);
            let subscribed = core.handle(EngineMsg::Subscribe(
                LiveQuery::from_filter(Filter::default()),
                Box::new(CapturingSink::default()),
            ));
            let handle = subscribed_handle(&subscribed);
            (core, handle)
        };
        let (mut direct, direct_handle) = make_core();
        let (mut oracle, oracle_handle) = make_core();

        let assert_same = |direct: &EngineCore<MemoryStore>, oracle: &EngineCore<MemoryStore>| {
            assert_remembered_rows_match_oracle(direct, direct_handle);
            assert_remembered_rows_match_oracle(oracle, oracle_handle);
            assert_eq!(
                direct.handles[&direct_handle].last_rows,
                oracle.handles[&oracle_handle].last_rows
            );
        };
        assert_same(&direct, &oracle);

        let winner = EventBuilder::new(Kind::ContactList, "new")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let (direct_replaceable_id, direct_replaceable) =
            apply_local_differential_accept(&mut direct, winner.clone(), 21, true);
        let (oracle_replaceable_id, oracle_replaceable) =
            apply_local_differential_accept(&mut oracle, winner, 21, false);
        assert_same(&direct, &oracle);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(30u64))
            .sign_with_keys(&keys)
            .unwrap();
        let (direct_deletion_id, direct_deletion) =
            apply_local_differential_accept(&mut direct, deletion.clone(), 31, true);
        let (oracle_deletion_id, oracle_deletion) =
            apply_local_differential_accept(&mut oracle, deletion, 31, false);
        assert_same(&direct, &oracle);

        let ordinary = room_event(&keys, 31, 1, 40);
        apply_local_differential_accept(&mut direct, ordinary.clone(), 41, true);
        apply_local_differential_accept(&mut oracle, ordinary, 41, false);
        assert_same(&direct, &oracle);

        apply_local_differential_compensation(
            &mut direct,
            direct_deletion_id,
            direct_deletion,
            true,
        );
        apply_local_differential_compensation(
            &mut oracle,
            oracle_deletion_id,
            oracle_deletion,
            false,
        );
        assert_same(&direct, &oracle);

        apply_local_differential_compensation(
            &mut direct,
            direct_replaceable_id,
            direct_replaceable,
            true,
        );
        apply_local_differential_compensation(
            &mut oracle,
            oracle_replaceable_id,
            oracle_replaceable,
            false,
        );
        assert_same(&direct, &oracle);

        apply_local_differential_expiry(&mut direct, Timestamp::from(100u64), true);
        apply_local_differential_expiry(&mut oracle, Timestamp::from(100u64), false);
        assert_same(&direct, &oracle);
        assert!(!direct.handles[&direct_handle]
            .last_rows
            .contains_key(&expiring.id));
    }

    #[test]
    fn ordinary_room_batch_queries_only_the_matching_handle_and_skips_router_compile() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://affected-room.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);

        let mut seed = Vec::new();
        for room in 0..HANDLE_COUNT {
            for ordinal in 0..ROWS_PER_HANDLE {
                let event = room_event(
                    &keys,
                    room,
                    ordinal,
                    (room * ROWS_PER_HANDLE + ordinal + 1) as u64,
                );
                seed.push((
                    event,
                    RelayObserved::new(relay.clone(), Timestamp::from(1u64)),
                ));
            }
        }
        core.resolver.store_mut().insert_batch(seed).unwrap();

        let sinks: Vec<_> = (0..HANDLE_COUNT)
            .map(|room| {
                let sink = CapturingSink::default();
                core.handle(EngineMsg::Subscribe(
                    room_query(room),
                    Box::new(sink.clone()),
                ));
                sink
            })
            .collect();

        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        for sink in &sinks {
            sink.0.lock().unwrap().clear();
        }

        let arriving = room_event(&keys, 17, 99, 50_000);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        for (room, sink) in sinks.iter().enumerate() {
            let batches = sink.0.lock().unwrap();
            if room == 17 {
                assert_eq!(batches.len(), 1);
                assert!(matches!(
                    batches[0].as_slice(),
                    [RowDelta::Added(row)] if row.event.id == arriving.id
                ));
            } else {
                assert!(batches.is_empty(), "unrelated room {room} was refreshed");
            }
        }

        // A byte-for-byte duplicate observation is a true no-op: no handle
        // query and no router compile merely to rediscover that fact.
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let mut duplicate_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(50_001u64)),
            )],
            &mut duplicate_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        assert!(duplicate_effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))));

        // The same id from a genuinely new relay changes only provenance.
        // The committed provenance fact is already exact: emit SourcesGrew
        // without re-querying prior room history, unrelated handles, or the
        // router.
        for sink in &sinks {
            sink.0.lock().unwrap().clear();
        }
        core.projection_store_queries.set(0);
        core.router_compiles.set(0);
        let second_relay = RelayUrl::parse("wss://second-room-source.example").unwrap();
        let mut provenance_effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(second_relay.clone(), Timestamp::from(50_002u64)),
            )],
            &mut provenance_effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert_eq!(core.router_compiles.get(), 0);
        for (room, sink) in sinks.iter().enumerate() {
            let batches = sink.0.lock().unwrap();
            if room == 17 {
                assert_eq!(batches.len(), 1);
                assert!(matches!(
                    batches[0].as_slice(),
                    [RowDelta::SourcesGrew { id, sources }]
                        if *id == arriving.id
                            && *sources == BTreeSet::from([relay.clone(), second_relay.clone()])
                ));
            } else {
                assert!(batches.is_empty(), "unrelated room {room} was refreshed");
            }
        }
    }

    #[test]
    fn top_n_insert_queries_only_its_handle_and_emits_eviction_delta() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let oldest = room_event(&keys, 7, 0, 10);
        let retained = room_event(&keys, 7, 1, 20);
        let unrelated = room_event(&keys, 8, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(
                [oldest.clone(), retained, unrelated]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(30u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();

        let affected = CapturingSink::default();
        let other = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(7, 9, 2),
            Box::new(affected.clone()),
        ));
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(8, 9, 2),
            Box::new(other.clone()),
        ));
        affected.0.lock().unwrap().clear();
        other.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let newest = room_event(&keys, 7, 2, 40);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                newest.clone(),
                RelayObserved::new(relay, Timestamp::from(41u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        let batches = affected.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == newest.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == oldest.id)));
        assert!(other.0.lock().unwrap().is_empty());
    }

    #[test]
    fn top_n_visible_removal_uses_one_bounded_backfill_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-backfill.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let oldest = room_event(&keys, 21, 0, 10);
        let middle = room_event(&keys, 21, 1, 20);
        let newest = room_event(&keys, 21, 2, 30);
        core.resolver
            .store_mut()
            .insert_batch(
                [oldest.clone(), middle, newest.clone()]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(31u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();

        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(21, 9, 2),
            Box::new(sink.clone()),
        ));
        sink.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(newest.id))
            .custom_created_at(Timestamp::from(40u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(deletion, RelayObserved::new(relay, Timestamp::from(41u64)))],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        let batches = sink.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == oldest.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == newest.id)));
    }

    #[test]
    fn top_n_equal_timestamp_id_tie_is_applied_without_store_read() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://top-n-tie.example").unwrap();
        let tied = |content: &str| {
            EventBuilder::new(Kind::from(9u16), content)
                .tag(Tag::parse(["h".to_owned(), "room-22".to_owned()]).unwrap())
                .custom_created_at(Timestamp::from(50u64))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let mut pair = [tied("a"), tied("b")];
        pair.sort_by_key(|event| event.id);
        let arriving = pair[0].clone();
        let seeded = pair[1].clone();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        core.resolver
            .store_mut()
            .insert(
                seeded.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(51u64)),
            )
            .unwrap();

        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query_for_kind(22, 9, 1),
            Box::new(sink.clone()),
        ));
        sink.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                arriving.clone(),
                RelayObserved::new(relay, Timestamp::from(52u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        let batches = sink.0.lock().unwrap();
        assert_eq!(batches.len(), 1);
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == arriving.id)));
        assert!(batches[0]
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == seeded.id)));
    }

    #[test]
    fn same_batch_insert_and_delete_is_a_zero_query_zero_delta_net_noop() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://same-batch-delete.example").unwrap();
        let target = room_event(&keys, 23, 0, 10);
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(room_query(23), Box::new(sink.clone())));
        sink.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![
                (
                    target,
                    RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
                ),
                (deletion, RelayObserved::new(relay, Timestamp::from(21u64))),
            ],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(sink.0.lock().unwrap().is_empty());
        assert!(effects
            .iter()
            .all(|effect| !matches!(effect, Effect::EmitRows(..))));
    }

    #[test]
    fn same_batch_multi_relay_insert_emits_complete_initial_sources_without_read() {
        let keys = Keys::generate();
        let first = RelayUrl::parse("wss://batch-source-a.example").unwrap();
        let second = RelayUrl::parse("wss://batch-source-b.example").unwrap();
        let event = room_event(&keys, 24, 0, 10);
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(room_query(24), Box::new(sink.clone())));
        sink.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![
                (
                    event.clone(),
                    RelayObserved::new(first.clone(), Timestamp::from(11u64)),
                ),
                (
                    event.clone(),
                    RelayObserved::new(second.clone(), Timestamp::from(12u64)),
                ),
            ],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(
                batch.as_slice(),
                [RowDelta::Added(row)]
                    if row.event.id == event.id
                        && row.sources == BTreeSet::from([first, second])
            )
        ));
    }

    #[test]
    fn replaceable_supersession_invalidates_old_and_new_matches_only() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://replaceable-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let replaceable = |room: usize, created_at: u64| {
            EventBuilder::new(Kind::from(10_000u16), format!("winner-{room}"))
                .tag(Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap())
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let old = replaceable(3, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                old.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        let old_sink = CapturingSink::default();
        let new_sink = CapturingSink::default();
        let unrelated_sink = CapturingSink::default();
        for (room, sink) in [
            (3, old_sink.clone()),
            (4, new_sink.clone()),
            (5, unrelated_sink.clone()),
        ] {
            core.handle(EngineMsg::Subscribe(
                room_query_for_kind(room, 10_000, 10),
                Box::new(sink.clone()),
            ));
            sink.0.lock().unwrap().clear();
        }
        core.projection_store_queries.set(0);

        let new = replaceable(4, 20);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                new.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );

        // Both windows were known incomplete (one row under limit 10), so
        // neither removal nor insertion can expose an unknown backfill.
        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            old_sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Removed(id)] if *id == old.id)
        ));
        assert!(matches!(
            new_sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Added(row)] if row.event.id == new.id)
        ));
        assert!(unrelated_sink.0.lock().unwrap().is_empty());
    }

    #[test]
    fn kind_five_removed_row_invalidates_matching_handle_without_shape_luck() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://deletion-affected.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let target = room_event(&keys, 12, 0, 10);
        core.resolver
            .store_mut()
            .insert_batch(vec![(
                target.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )])
            .unwrap();

        let affected = CapturingSink::default();
        let unrelated = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            room_query(12),
            Box::new(affected.clone()),
        ));
        core.handle(EngineMsg::Subscribe(
            room_query(13),
            Box::new(unrelated.clone()),
        ));
        affected.0.lock().unwrap().clear();
        unrelated.0.lock().unwrap().clear();
        core.projection_store_queries.set(0);

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target.id))
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(deletion, RelayObserved::new(relay, Timestamp::from(21u64)))],
            &mut effects,
        );

        // The prior window held one row under limit 200, proving no hidden
        // backfill candidate existed; the committed removal is exact.
        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            affected.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Removed(id)] if *id == target.id)
        ));
        assert!(unrelated.0.lock().unwrap().is_empty());
    }

    #[test]
    fn strict_pinned_projection_keeps_provenance_filtering_on_the_refresh_oracle() {
        let keys = Keys::generate();
        let pinned = RelayUrl::parse("wss://strict-pinned.example").unwrap();
        let other = RelayUrl::parse("wss://strict-other.example").unwrap();
        let LiveQuery(mut demand) = room_query(25);
        demand.source = SourceAuthority::Pinned(BTreeSet::from([pinned.clone()]));
        demand.cache = CacheMode::Strict;

        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(
            LiveQuery(demand),
            Box::new(sink.clone()),
        ));
        sink.0.lock().unwrap().clear();

        let event = room_event(&keys, 25, 0, 10);
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event.clone(),
                RelayObserved::new(other.clone(), Timestamp::from(11u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(sink.0.lock().unwrap().is_empty());

        core.projection_store_queries.set(0);
        core.ingest_relay_events(
            vec![(
                event.clone(),
                RelayObserved::new(pinned.clone(), Timestamp::from(12u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(
                batch.as_slice(),
                [RowDelta::Added(row)]
                    if row.event.id == event.id
                        && row.sources == BTreeSet::from([other, pinned])
            )
        ));
    }

    #[test]
    fn one_resolved_root_with_a_derived_subtree_uses_the_refresh_oracle() {
        let me = Keys::generate();
        let followed = Keys::generate();
        let relay = RelayUrl::parse("wss://derived-fallback.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let contact_list = EventBuilder::new(Kind::ContactList, "")
            .tag(Tag::public_key(followed.public_key()))
            .custom_created_at(Timestamp::from(10u64))
            .sign_with_keys(&me)
            .unwrap();
        core.resolver
            .store_mut()
            .insert(
                contact_list,
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )
            .unwrap();
        core.handle(EngineMsg::SetActivePubkey(Some(me.public_key())));

        let query = LiveQuery::from_filter(Filter {
            kinds: Some(BTreeSet::from([9u16])),
            authors: Some(Binding::Derived(Box::new(nmp_grammar::Derived {
                inner: nmp_grammar::Demand::from_filter(Filter {
                    kinds: Some(BTreeSet::from([3u16])),
                    authors: Some(Binding::Reactive(nmp_grammar::IdentityField::ActivePubkey)),
                    ..Filter::default()
                }),
                project: nmp_grammar::Selector::Tag("p".to_owned()),
            }))),
            ..Filter::default()
        });
        let sink = CapturingSink::default();
        core.handle(EngineMsg::Subscribe(query, Box::new(sink.clone())));
        sink.0.lock().unwrap().clear();

        let post = EventBuilder::new(Kind::from(9u16), "followed post")
            .custom_created_at(Timestamp::from(20u64))
            .sign_with_keys(&followed)
            .unwrap();
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                post.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );

        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Added(row)] if row.event.id == post.id)
        ));
    }

    #[test]
    fn incomplete_projection_forces_one_recovery_read_before_direct_deltas_resume() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://projection-recovery.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let sink = CapturingSink::default();
        let subscribed = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(28),
            Box::new(sink.clone()),
        ));
        let handle = subscribed_handle(&subscribed);
        sink.0.lock().unwrap().clear();
        core.handles.get_mut(&handle).unwrap().projection_complete = false;

        let first = room_event(&keys, 28, 0, 10);
        core.projection_store_queries.set(0);
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                first.clone(),
                RelayObserved::new(relay.clone(), Timestamp::from(11u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 1);
        assert!(core.handles[&handle].projection_complete);

        sink.0.lock().unwrap().clear();
        let second = room_event(&keys, 28, 1, 20);
        core.projection_store_queries.set(0);
        core.ingest_relay_events(
            vec![(
                second.clone(),
                RelayObserved::new(relay, Timestamp::from(21u64)),
            )],
            &mut effects,
        );
        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(matches!(
            sink.0.lock().unwrap().as_slice(),
            [batch] if matches!(batch.as_slice(), [RowDelta::Added(row)] if row.event.id == second.id)
        ));
    }

    #[test]
    fn fixed_seed_mixed_batches_match_a_forced_full_refresh_after_every_commit() {
        let keys = Keys::generate();
        let first = RelayUrl::parse("wss://differential-a.example").unwrap();
        let second = RelayUrl::parse("wss://differential-b.example").unwrap();
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let sink = CapturingSink::default();
        let subscribe = core.handle(EngineMsg::Subscribe(
            unlimited_room_query(26),
            Box::new(sink.clone()),
        ));
        let handle = subscribed_handle(&subscribe);
        sink.0.lock().unwrap().clear();
        let mut app_rows = BTreeMap::<EventId, Row>::new();
        let mut candidates = Vec::<SignedEvent>::new();
        let mut seed = 0x4d59_5df4_d0f3_3173u64;

        for step in 0..256u64 {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let created_at = 1_000 + step / 3;
            let observed_at = Timestamp::from(50_000 + step);
            let batch = match seed % 5 {
                0 => {
                    let event = room_event(&keys, 26, step as usize, created_at);
                    candidates.push(event.clone());
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
                1 if !candidates.is_empty() => {
                    let event = candidates[(seed as usize) % candidates.len()].clone();
                    vec![(event, RelayObserved::new(second.clone(), observed_at))]
                }
                2 if !candidates.is_empty() => {
                    let target = &candidates[(seed as usize) % candidates.len()];
                    let deletion = EventBuilder::new(Kind::EventDeletion, "")
                        .tag(Tag::event(target.id))
                        .custom_created_at(Timestamp::from(100_000 + step))
                        .sign_with_keys(&keys)
                        .unwrap();
                    vec![(deletion, RelayObserved::new(first.clone(), observed_at))]
                }
                3 => {
                    let event =
                        EventBuilder::new(Kind::from(10_000u16), format!("revision-{step}"))
                            .tag(Tag::parse(["h".to_owned(), "room-26".to_owned()]).unwrap())
                            .custom_created_at(Timestamp::from(200_000 + step))
                            .sign_with_keys(&keys)
                            .unwrap();
                    candidates.push(event.clone());
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
                _ => {
                    let event = room_event(&keys, 27, step as usize, created_at);
                    vec![(event, RelayObserved::new(first.clone(), observed_at))]
                }
            };

            core.projection_store_queries.set(0);
            let mut effects = Vec::new();
            core.ingest_relay_events(batch, &mut effects);
            assert_eq!(
                core.projection_store_queries.get(),
                0,
                "unlimited ordinary handle re-read history at step {step}"
            );

            let emitted = std::mem::take(&mut *sink.0.lock().unwrap());
            for delta in emitted.into_iter().flatten() {
                match delta {
                    RowDelta::Added(row) => {
                        app_rows.insert(row.event.id, row);
                    }
                    RowDelta::SourcesGrew { id, sources } => {
                        app_rows
                            .get_mut(&id)
                            .expect("source growth follows add")
                            .sources = sources;
                    }
                    RowDelta::Removed(id) => {
                        app_rows.remove(&id);
                    }
                }
            }

            assert_remembered_rows_match_oracle(&core, handle);
            let remembered = &core.handles[&handle].last_rows;
            assert_eq!(app_rows.len(), remembered.len());
            for (event_id, row) in &app_rows {
                assert_eq!(row.sources, remembered[event_id].sources);
            }
        }
    }

    #[test]
    fn resolver_internal_handle_is_filtered_before_any_projection_read() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let (internal, _delta) = core.resolver.subscribe(room_query(1)).unwrap();
        core.projection_store_queries.set(0);

        let mut effects = Vec::new();
        core.refresh_handles([internal.id()], &mut effects);

        assert_eq!(core.projection_store_queries.get(), 0);
        assert!(effects.is_empty());
    }

    #[test]
    fn projected_private_relay_evidence_is_gated_without_counter_inflation() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20);
        let private = RelayUrl::parse("ws://127.0.0.1:7777").unwrap();
        let atom = ContextualAtom {
            filter: ConcreteFilter {
                ids: Some(BTreeSet::from(["11".repeat(32)])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::from([RoutingEvidence {
                relay: private,
                origin: nmp_grammar::RoutingEvidenceKind::Hint,
            }]),
        };
        let demand = BTreeSet::from([atom]);

        let admitted = core.admit_projected_routing_evidence(&demand);
        assert!(admitted.iter().next().unwrap().routing_evidence.is_empty());
        assert_eq!(core.discovered_private_relays_rejected, 1);
        core.admit_projected_routing_evidence(&demand);
        assert_eq!(
            core.discovered_private_relays_rejected, 1,
            "an unchanged recompile must not recount one rejected fact"
        );
    }

    #[test]
    fn operator_allowlist_admits_projected_local_evidence() {
        let mut core = EngineCore::new(MemoryStore::new(), Box::new(FixtureDirectory::new()), 20)
            .with_relay_admission(RelayAdmissionPolicy::new(["127.0.0.1".to_string()]));
        let atom = ContextualAtom {
            filter: ConcreteFilter::default(),
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::from([RoutingEvidence {
                relay: RelayUrl::parse("ws://127.0.0.1:7777").unwrap(),
                origin: nmp_grammar::RoutingEvidenceKind::SourceProvenance,
            }]),
        };

        let admitted = core.admit_projected_routing_evidence(&BTreeSet::from([atom]));

        assert_eq!(admitted.iter().next().unwrap().routing_evidence.len(), 1);
        assert_eq!(core.discovered_private_relays_rejected, 0);
    }
}

#[cfg(test)]
mod history_mutation_tests {
    use std::sync::{Arc, Mutex};

    use nmp_grammar::{Derived, IdentityField, IndexedTagName, Selector};
    use nmp_router::FixtureDirectory;
    use nmp_store::MemoryStore;
    use nostr::{EventBuilder, Keys, Kind, Tag};

    use super::*;

    #[derive(Clone, Default)]
    struct CapturingHistorySink(Arc<Mutex<Vec<HistoryBatch>>>);

    impl HistorySink for CapturingHistorySink {
        fn on_history(&self, batch: HistoryBatch) {
            self.0.lock().unwrap().push(batch);
        }
    }

    #[derive(Clone, Default)]
    struct CapturingRowSink(Arc<Mutex<Vec<Vec<RowDelta>>>>);

    impl RowSink for CapturingRowSink {
        fn on_rows(&self, rows: Vec<RowDelta>) {
            self.0.lock().unwrap().push(rows);
        }
    }

    #[derive(Clone, Default)]
    struct CapturingReceiptSink(Arc<Mutex<Vec<WriteStatus>>>);

    impl ReceiptSink for CapturingReceiptSink {
        fn on_status(&self, status: WriteStatus) {
            self.0.lock().unwrap().push(status);
        }
    }

    fn room_tag(room: usize) -> Tag {
        Tag::parse(["h".to_owned(), format!("room-{room}")]).unwrap()
    }

    fn room_event(keys: &Keys, room: usize, ordinal: usize, created_at: u64) -> SignedEvent {
        EventBuilder::new(Kind::from(9u16), format!("room-{room}-{ordinal}"))
            .tag(room_tag(room))
            .custom_created_at(Timestamp::from(created_at))
            .sign_with_keys(keys)
            .unwrap()
    }

    fn history_query(room: usize, kinds: BTreeSet<u16>) -> HistoryQuery {
        HistoryQuery::new(
            LiveQuery::from_filter(Filter {
                kinds: Some(kinds),
                tags: BTreeMap::from([(
                    IndexedTagName::new('h').unwrap(),
                    Binding::Literal(BTreeSet::from([format!("room-{room}")])),
                )]),
                ..Filter::default()
            }),
            3,
            6,
        )
    }

    fn open_six(
        events: &[SignedEvent],
        kinds: BTreeSet<u16>,
        relay: &RelayUrl,
    ) -> (
        EngineCore<MemoryStore>,
        HistorySessionId,
        CapturingHistorySink,
    ) {
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                events
                    .iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, kinds),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let loaded = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(loaded.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        (core, id, sink)
    }

    fn ordered_ids<S: EventStore>(core: &EngineCore<S>, id: HistorySessionId) -> Vec<EventId> {
        core.histories[&id]
            .order
            .iter()
            .map(|(_, event_id)| *event_id)
            .collect()
    }

    fn ingest<S: EventStore>(
        core: &mut EngineCore<S>,
        event: SignedEvent,
        relay: RelayUrl,
        observed_at: u64,
    ) {
        let mut effects = Vec::new();
        core.ingest_relay_events(
            vec![(
                event,
                RelayObserved::new(relay, Timestamp::from(observed_at)),
            )],
            &mut effects,
        );
    }

    fn assert_one_atomic_batch(sink: &CapturingHistorySink) -> HistoryBatch {
        let batches = sink.0.lock().unwrap();
        assert_eq!(
            batches.len(),
            1,
            "one store commit must emit one history batch"
        );
        batches[0].clone()
    }

    #[test]
    fn bounded_history_mutations_touch_only_delta_and_exact_lower_segment() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-mutation.example").unwrap();
        let second = RelayUrl::parse("wss://history-second.example").unwrap();
        let base: Vec<_> = (0..12)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();

        // First boundary insertion is old-window + inserted -> top-N: no
        // store read, and Added+Removed travel in one atomic batch.
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let inserted = room_event(&keys, 47, 99, 1_000);
        ingest(&mut core, inserted.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == inserted.id)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(_))));
        assert_eq!(core.histories[&id].last_rows.len(), 6);

        // Middle provenance growth is exact from the committed fact.
        let middle = ordered_ids(&core, id)[2];
        let middle_event = core
            .resolver
            .store()
            .query(&nostr::Filter::new().id(middle))
            .unwrap()
            .pop()
            .unwrap()
            .event;
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, middle_event, second.clone(), 2_001);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (0, 0)
        );
        assert!(matches!(
            batch.deltas.as_slice(),
            [RowDelta::SourcesGrew { id: changed, sources }]
                if *changed == middle && sources.contains(&relay) && sources.contains(&second)
        ));

        // Middle deletion performs one exclusive cursor read for exactly one
        // replacement row; it never replays all six retained rows.
        let target = ordered_ids(&core, id)[2];
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay.clone(), 3_001);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == target)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(_))));

        // The lower boundary uses the same one-row segment, proving cursor
        // work does not depend on retained-window size.
        let target = *ordered_ids(&core, id).last().unwrap();
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(target))
            .custom_created_at(Timestamp::from(3_100u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay.clone(), 3_101);
        assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
    }

    #[test]
    fn strict_history_counts_only_pinned_provenance_before_applying_page_bounds() {
        let keys = Keys::generate();
        let wanted = RelayUrl::parse("wss://history-strict.example").unwrap();
        let other = RelayUrl::parse("wss://history-other.example").unwrap();
        let mut store = MemoryStore::new();
        for (created_at, relay, ordinal) in [
            (600, other.clone(), 0),
            (500, other.clone(), 1),
            (400, wanted.clone(), 2),
            (300, wanted.clone(), 3),
            (200, wanted.clone(), 4),
            (100, wanted.clone(), 5),
        ] {
            store
                .insert(
                    room_event(&keys, 47, ordinal, created_at),
                    RelayObserved::new(relay, Timestamp::from(1_000u64)),
                )
                .unwrap();
        }
        let selection = history_query(47, BTreeSet::from([9]))
            .live_query()
            .0
            .selection
            .clone();
        let query = HistoryQuery::new(
            LiveQuery(nmp_grammar::Demand {
                selection,
                source: SourceAuthority::Pinned(BTreeSet::from([wanted])),
                access: AccessContext::Public,
                cache: CacheMode::Strict,
            }),
            2,
            4,
        );
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            ordered_ids(&core, id)
                .iter()
                .map(|event_id| {
                    core.histories[&id].last_rows[event_id]
                        .event
                        .created_at
                        .as_secs()
                })
                .collect::<Vec<_>>(),
            vec![400, 300]
        );

        core.handle(EngineMsg::RequestRows(id, 4));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(
            ordered_ids(&core, id)
                .iter()
                .map(|event_id| {
                    core.histories[&id].last_rows[event_id]
                        .event
                        .created_at
                        .as_secs()
                })
                .collect::<Vec<_>>(),
            vec![400, 300, 200, 100]
        );
    }

    #[test]
    fn strict_and_agnostic_live_mutations_stay_distinct_and_match_their_oracles() {
        let keys = Keys::generate();
        let wanted = RelayUrl::parse("wss://history-live-wanted.example").unwrap();
        let other = RelayUrl::parse("wss://history-live-other.example").unwrap();
        let other_newest = room_event(&keys, 47, 0, 400);
        let wanted_a = room_event(&keys, 47, 1, 300);
        let wanted_b = room_event(&keys, 47, 2, 200);
        let wanted_c = room_event(&keys, 47, 3, 100);
        let mut store = MemoryStore::new();
        for (event, source) in [
            (other_newest.clone(), other.clone()),
            (wanted_a.clone(), wanted.clone()),
            (wanted_b.clone(), wanted.clone()),
            (wanted_c.clone(), wanted.clone()),
        ] {
            store
                .insert(event, RelayObserved::new(source, Timestamp::from(1_000u64)))
                .unwrap();
        }
        let selection = history_query(47, BTreeSet::from([9]))
            .live_query()
            .0
            .selection
            .clone();
        let strict_query = HistoryQuery::new(
            LiveQuery(nmp_grammar::Demand {
                selection,
                source: SourceAuthority::Pinned(BTreeSet::from([wanted.clone()])),
                access: AccessContext::Public,
                cache: CacheMode::Strict,
            }),
            3,
            3,
        );
        let agnostic_query = HistoryQuery::new(
            history_query(47, BTreeSet::from([9])).live_query().clone(),
            3,
            3,
        );
        let strict_sink = CapturingHistorySink::default();
        let agnostic_sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let strict_id = core
            .handle(EngineMsg::SubscribeHistory(
                strict_query,
                Box::new(strict_sink.clone()),
            ))
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let agnostic_id = core
            .handle(EngineMsg::SubscribeHistory(
                agnostic_query,
                Box::new(agnostic_sink.clone()),
            ))
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(
            ordered_ids(&core, strict_id),
            vec![wanted_a.id, wanted_b.id, wanted_c.id]
        );
        assert_eq!(
            ordered_ids(&core, agnostic_id),
            vec![other_newest.id, wanted_a.id, wanted_b.id]
        );
        strict_sink.0.lock().unwrap().clear();
        agnostic_sink.0.lock().unwrap().clear();

        let new = room_event(&keys, 47, 99, 500);
        ingest(&mut core, new.clone(), other.clone(), 2_000);
        assert!(strict_sink.0.lock().unwrap().is_empty());
        assert_eq!(ordered_ids(&core, strict_id)[0], wanted_a.id);
        assert_eq!(ordered_ids(&core, agnostic_id)[0], new.id);

        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        core.history_affected_row_queries.set(0);
        ingest(&mut core, new.clone(), wanted.clone(), 2_001);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert_eq!(core.history_affected_row_queries.get(), 1);
        assert_eq!(ordered_ids(&core, strict_id)[0], new.id);
        let strict_new = &core.histories[&strict_id].last_rows[&new.id];
        assert_eq!(
            strict_new.sources,
            BTreeSet::from([other.clone(), wanted.clone()]),
            "a newly Strict-eligible row carries its complete canonical provenance"
        );

        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(new.id))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        strict_sink.0.lock().unwrap().clear();
        agnostic_sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, wanted, 3_001);
        assert_eq!(core.history_store_queries.get(), 2);
        assert_eq!(core.history_rows_examined.get(), 2);
        assert_eq!(strict_sink.0.lock().unwrap().len(), 1);
        assert_eq!(agnostic_sink.0.lock().unwrap().len(), 1);

        for history_id in [strict_id, agnostic_id] {
            let (oracle, _) = core.history_rows_and_evidence_for(history_id).unwrap();
            assert_eq!(core.histories[&history_id].last_rows, oracle);
        }
    }

    #[test]
    fn replacement_and_expiry_rebalance_without_full_history_replay() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-replace-expire.example").unwrap();
        let mut base: Vec<_> = (0..11)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let replaceable = EventBuilder::new(Kind::from(10_000u16), "old")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(108u64))
            .sign_with_keys(&keys)
            .unwrap();
        base.push(replaceable.clone());
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9, 10_000]), &relay);
        assert!(core.histories[&id].last_rows.contains_key(&replaceable.id));
        let replacement = EventBuilder::new(Kind::from(10_000u16), "new")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(1_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        ingest(&mut core, replacement.clone(), relay.clone(), 2_000);
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == replaceable.id)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == replacement.id)));

        let expiring = EventBuilder::new(Kind::from(9u16), "expires")
            .tag(room_tag(47))
            .tag(Tag::expiration(Timestamp::from(5_000u64)))
            .custom_created_at(Timestamp::from(900u64))
            .sign_with_keys(&keys)
            .unwrap();
        sink.0.lock().unwrap().clear();
        ingest(&mut core, expiring.clone(), relay, 2_001);
        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        core.tick(Timestamp::from(5_000u64));
        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1)
        );
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == expiring.id)));
    }

    #[test]
    fn replaceable_compensation_cannot_let_restored_older_row_mask_hidden_tail() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-compensation.example").unwrap();
        let x = room_event(&keys, 47, 1, 900);
        let y = room_event(&keys, 47, 2, 800);
        let z = room_event(&keys, 47, 3, 700);
        let predecessor = EventBuilder::new(Kind::from(10_000u16), "prior")
            .tag(room_tag(47))
            .custom_created_at(Timestamp::from(100u64))
            .sign_with_keys(&keys)
            .unwrap();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                [x.clone(), y.clone(), z.clone(), predecessor.clone()]
                    .into_iter()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.active_pubkey = Some(keys.public_key());
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9, 10_000])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);
        sink.0.lock().unwrap().clear();

        let accepted = core.on_publish(
            WriteIntent {
                payload: WritePayload::UnsignedReplaceableEdit {
                    unsigned: UnsignedEvent::new(
                        keys.public_key(),
                        Timestamp::from(1_000u64),
                        Kind::from(10_000u16),
                        vec![room_tag(47)],
                        "pending replacement",
                    ),
                    expected_base: Some(predecessor.id),
                },
                durability: Durability::Durable,
                routing: WriteRouting::PinnedHost(HostAuthority::from_selected_host(relay)),
                identity_override: None,
            },
            Box::new(CapturingReceiptSink::default()),
        );
        let receipt = accepted
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitReceipt(id, WriteStatus::Accepted) => Some(*id),
                _ => None,
            })
            .expect("replaceable local acceptance emits a receipt");
        let pending = *ordered_ids(&core, id).first().unwrap();
        assert_eq!(ordered_ids(&core, id)[1..], [x.id, y.id]);

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        core.on_cancel_write(receipt);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(
            (
                core.history_store_queries.get(),
                core.history_rows_examined.get()
            ),
            (1, 1),
            "one old-boundary reconciliation finds Z despite predecessor restoring count"
        );
        assert_eq!(ordered_ids(&core, id), vec![x.id, y.id, z.id]);
        assert!(!core.histories[&id].last_rows.contains_key(&predecessor.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Removed(id) if *id == pending)));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == z.id)));
        assert!(!batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == predecessor.id)));
    }

    #[test]
    fn fixed_seed_mixed_remove_insert_batches_match_full_history_oracle() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-differential.example").unwrap();
        let base: Vec<_> = (0..30)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let mut seed = 0x6a09_e667_f3bc_c909u64;

        for step in 0..64usize {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            let visible = ordered_ids(&core, id);
            let removed_id = visible[(seed as usize) % visible.len()];
            let removed = core
                .resolver
                .store()
                .query(&nostr::Filter::new().id(removed_id))
                .unwrap()
                .pop()
                .unwrap()
                .event;
            core.resolver
                .store_mut()
                .remove(removed_id, nmp_store::RetractReason::Rejected)
                .unwrap();

            seed = seed.rotate_left(17) ^ 0xa5a5_5a5a_0123_4567;
            let created_at = 50 + (seed % 1_500);
            let inserted = room_event(&keys, 47, 10_000 + step, created_at);
            core.resolver
                .store_mut()
                .insert(
                    inserted.clone(),
                    RelayObserved::new(relay.clone(), Timestamp::from(2_000 + step as u64)),
                )
                .unwrap();
            let changes = CommittedRowChanges {
                inserted: vec![nmp_resolver::CommittedCurrentRow {
                    event: inserted,
                    observed_relays: BTreeSet::from([relay.clone()]),
                }],
                removed: vec![removed],
                provenance_grew: Vec::new(),
            };

            sink.0.lock().unwrap().clear();
            core.history_store_queries.set(0);
            core.history_rows_examined.set(0);
            let mut effects = Vec::new();
            assert!(core.try_apply_committed_history_row_changes(id, &changes, &mut effects));
            assert!(core.history_store_queries.get() <= 1);
            assert!(core.history_rows_examined.get() <= 1);
            assert!(sink.0.lock().unwrap().len() <= 1);

            let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
            assert_eq!(
                core.histories[&id].last_rows, oracle,
                "incremental history diverged from full oracle at mixed batch {step}"
            );
        }
    }

    #[test]
    fn derived_multi_root_advanced_history_mutates_with_one_bounded_reconciliation() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-multi-root.example").unwrap();
        let addressable = |d: &str, created_at: u64, content: &str| {
            EventBuilder::new(Kind::from(30_003u16), content)
                .tag(Tag::identifier(d))
                .custom_created_at(Timestamp::from(created_at))
                .sign_with_keys(&keys)
                .unwrap()
        };
        let base: Vec<_> = (0..8)
            .map(|index| addressable(&format!("g{index}"), 100 + index, "base"))
            .collect();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                base.iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let selection = nmp_grammar::Filter {
            authors: Some(Binding::Derived(Box::new(Derived {
                inner: nmp_grammar::Demand::from_filter(nmp_grammar::Filter {
                    kinds: Some(BTreeSet::from([30_003u16])),
                    authors: Some(Binding::Reactive(IdentityField::ActivePubkey)),
                    ..nmp_grammar::Filter::default()
                }),
                project: Selector::AddressCoord,
            }))),
            ..nmp_grammar::Filter::default()
        };
        let query = HistoryQuery::new(LiveQuery::from_filter(selection), 3, 6);
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        core.handle(EngineMsg::SetActivePubkey(Some(keys.public_key())));
        let opened = core.handle(EngineMsg::SubscribeHistory(query, Box::new(sink.clone())));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        let primary = *core.histories[&id].handle_ids.first().unwrap();
        assert_eq!(core.resolver.root_atoms(primary).len(), 8);
        assert!(core.resolver.subtree_atoms(primary).len() > 8);

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        let replacement = addressable("g7", 1_000, "replacement");
        ingest(&mut core, replacement.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 1);
        assert!(core.history_rows_examined.get() <= 1);
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == replacement.id)));
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn late_same_second_boundary_insert_after_advance_is_exact_and_read_free() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-late-tie.example").unwrap();
        let base: Vec<_> = [600u64, 500, 400, 300, 200, 100]
            .into_iter()
            .enumerate()
            .map(|(index, created_at)| room_event(&keys, 47, index, created_at))
            .collect();
        let old_boundary = base.last().unwrap().clone();
        let (mut core, id, sink) = open_six(&base, BTreeSet::from([9]), &relay);
        let late = (0..1_000usize)
            .map(|ordinal| room_event(&keys, 47, 20_000 + ordinal, 100))
            .find(|event| event.id < old_boundary.id)
            .expect("deterministically find an id that sorts before the old tie boundary");

        sink.0.lock().unwrap().clear();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, late.clone(), relay, 2_000);

        let batch = assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 0);
        assert_eq!(core.history_rows_examined.get(), 0);
        assert!(core.histories[&id].last_rows.contains_key(&late.id));
        assert!(!core.histories[&id].last_rows.contains_key(&old_boundary.id));
        assert!(batch
            .deltas
            .iter()
            .any(|delta| matches!(delta, RowDelta::Added(row) if row.event.id == late.id)));
        assert!(batch.deltas.iter().any(
            |delta| matches!(delta, RowDelta::Removed(event_id) if *event_id == old_boundary.id)
        ));
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn redb_advanced_history_matches_oracle_after_insert_and_retraction() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-redb.example").unwrap();
        let base: Vec<_> = (0..12)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let mut store = nmp_store::RedbStore::open(dir.path().join("history.redb")).unwrap();
        store
            .insert_batch(
                base.iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(id, 6));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        sink.0.lock().unwrap().clear();

        let inserted = room_event(&keys, 47, 99, 1_000);
        ingest(&mut core, inserted, relay.clone(), 2_000);
        sink.0.lock().unwrap().clear();
        let removed = ordered_ids(&core, id)[2];
        let deletion = EventBuilder::new(Kind::EventDeletion, "")
            .tag(Tag::event(removed))
            .custom_created_at(Timestamp::from(3_000u64))
            .sign_with_keys(&keys)
            .unwrap();
        core.history_store_queries.set(0);
        core.history_rows_examined.set(0);
        ingest(&mut core, deletion, relay, 3_001);

        assert_one_atomic_batch(&sink);
        assert_eq!(core.history_store_queries.get(), 1);
        assert_eq!(core.history_rows_examined.get(), 1);
        let (oracle, _) = core.history_rows_and_evidence_for(id).unwrap();
        assert_eq!(core.histories[&id].last_rows, oracle);
    }

    #[test]
    fn staged_load_rollback_and_cancel_restore_exact_session_ownership() {
        let keys = Keys::generate();
        let relay = RelayUrl::parse("wss://history-rollback.example").unwrap();
        let events: Vec<_> = (0..9)
            .map(|index| room_event(&keys, 47, index, 100 + index as u64))
            .collect();
        let mut store = MemoryStore::new();
        store
            .insert_batch(
                events
                    .iter()
                    .cloned()
                    .map(|event| {
                        (
                            event,
                            RelayObserved::new(relay.clone(), Timestamp::from(1_000u64)),
                        )
                    })
                    .collect(),
            )
            .unwrap();
        let sink = CapturingHistorySink::default();
        let mut core = EngineCore::new(store, Box::new(FixtureDirectory::new()), 20);
        let opened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(sink.clone()),
        ));
        let id = opened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        let row_sink = CapturingRowSink::default();
        let ordinary = core.handle(EngineMsg::Subscribe(
            history_query(47, BTreeSet::from([9])).live_query().clone(),
            Box::new(row_sink.clone()),
        ));
        let ordinary_id = ordinary
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitRows(handle, _, _) => Some(*handle),
                _ => None,
            })
            .unwrap();
        let second_sink = CapturingHistorySink::default();
        let second_open = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(second_sink.clone()),
        ));
        let second_id = second_open
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(candidate, _) if *candidate != id => Some(*candidate),
                _ => None,
            })
            .unwrap();
        sink.0.lock().unwrap().clear();
        row_sink.0.lock().unwrap().clear();
        second_sink.0.lock().unwrap().clear();

        let prior_rows = core.histories[&id].last_rows.clone();
        let prior_order = core.histories[&id].order.clone();
        let prior_evidence = core.histories[&id].last_evidence.clone();
        let prior_handles = core.histories[&id].handle_ids.clone();
        let ordinary_prior_rows = core.handles[&ordinary_id].last_rows.clone();
        let ordinary_prior_evidence = core.handles[&ordinary_id].last_evidence.clone();
        let second_prior_rows = core.histories[&second_id].last_rows.clone();
        let second_prior_evidence = core.histories[&second_id].last_evidence.clone();
        let second_prior_handles = core.histories[&second_id].handle_ids.clone();

        // A staged advance mutates only this session's retained projection
        // and is observable on NO sink until commit; every other projection
        // is untouched.
        let staged = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(staged.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        assert!(core.histories[&id].pending_load.is_some());
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        assert!(
            sink.0.lock().unwrap().is_empty(),
            "staged rows are not observable"
        );
        assert!(row_sink.0.lock().unwrap().is_empty());
        assert!(second_sink.0.lock().unwrap().is_empty());
        assert_eq!(core.handles[&ordinary_id].last_rows, ordinary_prior_rows);
        assert_eq!(
            core.handles[&ordinary_id].last_evidence,
            ordinary_prior_evidence
        );
        assert_eq!(core.histories[&second_id].last_rows, second_prior_rows);
        assert_eq!(
            core.histories[&second_id].last_evidence,
            second_prior_evidence
        );
        assert_eq!(core.histories[&second_id].handle_ids, second_prior_handles);

        core.handle(EngineMsg::RollbackHistoryLoad(id));
        let state = &core.histories[&id];
        assert_eq!(state.last_rows, prior_rows);
        assert_eq!(state.order, prior_order);
        assert_eq!(state.last_evidence, prior_evidence);
        assert_eq!(state.target_rows, 3);
        assert_eq!(state.handle_ids, prior_handles);
        assert!(state.pending_load.is_none());
        assert!(row_sink.0.lock().unwrap().is_empty());
        assert!(second_sink.0.lock().unwrap().is_empty());

        // The identical declarative request retries cleanly after rollback.
        let retried = core.handle(EngineMsg::RequestRows(id, 6));
        assert!(retried.iter().any(|effect| matches!(
            effect,
            Effect::HistoryLoadResult(session, Ok(())) if *session == id
        )));
        core.handle(EngineMsg::CommitHistoryLoad(id));
        assert_eq!(core.histories[&id].last_rows.len(), 6);
        let delivered = sink.0.lock().unwrap();
        assert_eq!(delivered.len(), 2);
        assert_eq!(delivered[0].load, WindowLoad::Requesting);
        assert_eq!(delivered[1].load, WindowLoad::Returned { added: 3 });
        assert_eq!(
            delivered[1]
                .evidence
                .shortfall
                .iter()
                .filter(|fact| matches!(fact, ShortfallFact::NoPlannedSource { .. }))
                .count(),
            3,
            "initial, exact tie-second, and older handles all contribute evidence"
        );
        drop(delivered);

        let owned_handles = core.histories[&id].handle_ids.clone();
        core.handle(EngineMsg::UnsubscribeHistory(id));
        assert!(!core.histories.contains_key(&id));
        assert!(core.history_by_handle.values().all(|owner| *owner != id));
        for handle in owned_handles {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }

        let active_sink = CapturingHistorySink::default();
        let reopened = core.handle(EngineMsg::SubscribeHistory(
            history_query(47, BTreeSet::from([9])),
            Box::new(active_sink),
        ));
        let active_id = reopened
            .iter()
            .find_map(|effect| match effect {
                Effect::EmitHistory(id, _) => Some(*id),
                _ => None,
            })
            .unwrap();
        core.handle(EngineMsg::RequestRows(active_id, 6));
        let active_handles = core.histories[&active_id].handle_ids.clone();
        assert!(core.histories[&active_id].pending_load.is_some());
        core.handle(EngineMsg::UnsubscribeHistory(active_id));
        assert!(!core.histories.contains_key(&active_id));
        assert!(core
            .history_by_handle
            .values()
            .all(|owner| *owner != active_id));
        for handle in active_handles {
            assert!(core.resolver.root_atoms(handle).is_empty());
        }
    }
}
