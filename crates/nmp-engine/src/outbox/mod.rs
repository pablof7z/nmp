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

use std::collections::BTreeSet;

use nostr::{Event as SignedEvent, EventId, PublicKey, RelayUrl, UnsignedEvent};

use crate::core::ReceiptId;

/// A typed property of a write (M0 amendment) — not a routing choice.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Durability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// The event payload of a write intent. VISION P states signing and
/// publishing are ORTHOGONAL stages, not one linear lifecycle: a caller
/// that already holds a validly-signed event (e.g. republishing a
/// previously-signed private event to a recomputed narrow relay set,
/// ledger #6) supplies `Signed` and skips `Effect::RequestSign` entirely,
/// going straight to routing; a caller with a template supplies `Unsigned`
/// and the reducer requests the signer capability.
pub enum WritePayload {
    Unsigned(UnsignedEvent),
    Signed(SignedEvent),
}

/// A caller's publish request.
pub struct WriteIntent {
    pub payload: WritePayload,
    pub durability: Durability,
    pub routing: WriteRouting,
}

/// Where a `WriteIntent` is routed.
#[derive(Clone)]
pub enum WriteRouting {
    /// The author's write relays (reuses the M2 router's lanes).
    AuthorOutbox,
    /// Recipients' inboxes (kind:10050 / NIP-65 read).
    ToInboxes(Vec<PublicKey>),
    /// Ledger #6: narrow-only, fail-closed.
    PrivateNarrow(PrivateRoute),
}

/// Fail-closed narrow relay set (ledger #6). By construction this type
/// exposes no widen/insert-arbitrary operation: `new` is the ONLY way to
/// populate it (a one-shot, fixed set at construction time — the caller
/// must already have resolved and narrowed this itself), and no
/// insert/extend/union method exists afterward. A `PrivateNarrow` intent
/// whose set is empty is exactly how an unroutable private recipient is
/// expressed structurally — the reducer fails it CLOSED (`WriteStatus::
/// Failed`), it never falls back to a public write relay, because there is
/// no operation that could hand it one.
#[derive(Debug, Clone, Default)]
pub struct NarrowOnly<T> {
    items: BTreeSet<T>,
}

impl<T: Ord> NarrowOnly<T> {
    /// Construct a narrow, FIXED relay set. No widen operation exists on
    /// this type — an empty set is legal and is how "unroutable" is
    /// expressed.
    pub fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            items: items.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, T> {
        self.items.iter()
    }
}

#[derive(Clone)]
pub struct PrivateRoute {
    pub relays: NarrowOnly<RelayUrl>,
}

/// The receipt STREAM (never bool/void on the durable path, ledger #9:
/// enqueue is not converged).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WriteStatus {
    Accepted,
    AwaitingCapability,
    Signed(EventId),
    Routed(BTreeSet<RelayUrl>),
    Sent(RelayUrl),
    Acked(RelayUrl),
    Rejected(RelayUrl, String),
    GaveUp(RelayUrl),
    /// The relay remains an owned, nonterminal delivery lane, but the
    /// durable `AttemptOutcome::Started` fact could not be committed. No
    /// wire EVENT was emitted. Recovery rediscovers the exact URL from its
    /// committed route revision; #79 owns when an in-process retry occurs.
    PersistenceBlocked(RelayUrl),
    /// The resolved relay is known in this process, but the append-only
    /// route revision itself could not be committed. No attempt or wire EVENT
    /// exists. Unlike `PersistenceBlocked`, this exact URL is not claimed to
    /// survive a crash.
    RoutePersistenceBlocked(RelayUrl),
    /// An at-most-once attempt crossed a process-loss boundary after its
    /// Started fact committed. Terminal ambiguity, never retry permission.
    OutcomeUnknown(RelayUrl),
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
