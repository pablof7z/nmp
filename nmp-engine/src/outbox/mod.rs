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
//! Step 0 declares the vocabulary only. D (depends on B + A3) wires
//! enqueue/route/sign-orchestration/per-relay-ack.

use std::collections::BTreeSet;

use nostr::{EventId, PublicKey, RelayUrl, UnsignedEvent};

use crate::core::ReceiptId;

/// A typed property of a write (M0 amendment) — not a routing choice.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Durability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// A caller's publish request.
pub struct WriteIntent {
    pub unsigned: UnsignedEvent,
    pub durability: Durability,
    pub routing: WriteRouting,
}

/// Where a `WriteIntent` is routed.
pub enum WriteRouting {
    /// The author's write relays (reuses the M2 router's lanes).
    AuthorOutbox,
    /// Recipients' inboxes (kind:10050 / NIP-65 read).
    ToInboxes(Vec<PublicKey>),
    /// Ledger #6: narrow-only, fail-closed.
    PrivateNarrow(PrivateRoute),
}

/// Fail-closed narrow relay set (ledger #6). By construction this type
/// exposes no widen/insert-arbitrary operation — D must not add one; a
/// `PrivateNarrow` intent whose route is unresolvable fails closed
/// (`Rejected`), it never falls back to a public write relay.
///
/// Step 0 leaves the field private and unread (no constructor yet) — that
/// opacity is the point, not an oversight.
#[allow(dead_code)]
pub struct NarrowOnly<T> {
    items: BTreeSet<T>,
}

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
