//! The PURE synchronous reducer (plan §2 position 1, §3.4). `EngineCore`
//! owns the M1 resolver `Engine<S>`, the M2 `Router`, the write-outbox
//! state, and the negentropy prober state. Its entire surface is:
//!
//! ```ignore
//! impl EngineCore {
//!     pub fn handle(&mut self, msg: EngineMsg) -> Vec<Effect>;
//!     pub fn tick(&mut self, now: nostr::Timestamp) -> Vec<Effect>;
//! }
//! ```
//!
//! `EngineCore` does NO I/O, spawns no threads, touches no socket, imposes
//! no runtime — this is the seam that preserves M1/M2's headless property:
//! the whole engine's logic is testable by feeding `EngineMsg`s and
//! asserting `Effect`s, with zero network (plan §5 tier A).
//!
//! Step 0 declares [`EngineMsg`]/[`Effect`] and the small supporting types
//! only. B (depends on A1) fills in `EngineCore`'s fields and the
//! `handle`/`tick` bodies per the reducer flow in plan §3.4.

use nostr::{Event as SignedEvent, EventId, PublicKey, RelayUrl, Timestamp, UnsignedEvent};

use nmp_grammar::DescriptorHash;
use nmp_resolver::{HandleId, LiveQuery};
use nmp_router::{WireDelta, WireReq};
use nmp_signer::SignerError;
use nmp_transport::{RelayFrame, RelayHandle};

use crate::negentropy::ProbedRelay;
use crate::outbox::{ReceiptSink, WriteIntent, WriteStatus};

/// Opaque id correlating a `Publish`/`RequestSign` to its `EmitReceipt`/
/// `SignerCompleted`.
#[derive(Debug, Clone, Copy, Eq, Hash, PartialEq, PartialOrd, Ord)]
pub struct ReceiptId(pub u64);

/// Sink an app-facing `Handle` registers for row deltas on a subscription.
pub trait RowSink: Send {
    fn on_rows(&self, rows: Vec<RowDelta>);
}

/// A raw row delta (plan §7 non-goal: no ordering/windowing in M3 — raw
/// deltas + coverage only). The per-relay coverage variant this carries is
/// deliberately NOT sketched here yet: plan §8 flags the coverage
/// attribution key (wide-wire-filter hash vs. narrow-atom hash) as the one
/// genuinely non-mechanical M3 decision, to be settled before B lands
/// `EmitRows` coverage.
pub struct RowDelta {
    pub event: nostr::Event,
}

/// The read/write/frame vocabulary the reducer consumes (plan §3.4).
pub enum EngineMsg {
    Subscribe(LiveQuery, Box<dyn RowSink>),
    Unsubscribe(HandleId),
    SetActivePubkey(Option<PublicKey>),
    Publish(WriteIntent, Box<dyn ReceiptSink>),
    RelayConnected(RelayHandle, RelayUrl),
    RelayDisconnected(u32),
    RelayFrame(RelayHandle, RelayFrame),
    SignerCompleted(ReceiptId, Result<SignedEvent, SignerError>),
    Tick(Timestamp),
}

/// The row/wire/receipt vocabulary the reducer emits (plan §3.4).
pub enum Effect {
    /// -> `Pool::send` per (relay, current handle).
    Wire(WireDelta),
    /// Reconnect: resend the current wire subs on the NEW generation.
    Replay(RelayUrl, Vec<WireReq>),
    StartProbe(RelayUrl),
    NegOpen(ProbedRelay, nmp_grammar::ConcreteFilter),
    RecordCoverage(DescriptorHash, RelayUrl, Timestamp),
    EmitRows(HandleId, Vec<RowDelta>),
    EmitReceipt(ReceiptId, WriteStatus),
    RequestSign(ReceiptId, UnsignedEvent),
    RequestDecrypt(EventId, PublicKey, String),
}

/// The PURE synchronous reducer (§2 position 1). No I/O, no threads.
///
/// Step 0: empty shell. B wires in the M1 resolver `Engine<S>` + M2
/// `Router` + write-outbox + prober state, and implements `handle`/`tick`
/// per the reducer flow documented in plan §3.4.
pub struct EngineCore;
