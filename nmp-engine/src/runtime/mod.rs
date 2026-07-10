//! The async edge (plan §2 position 2). `EngineThread` spawns ONE
//! dedicated OS thread that runs a blocking `recv` loop over an `mpsc`
//! inbox (D8: blocking recv / callbacks, never poll), calls
//! `core::EngineCore::handle`/`::tick`, and dispatches the returned
//! `core::Effect`s to `nmp_transport::Pool::send`, the `nmp_signer`
//! capabilities, and the app-facing sinks. `nmp_transport::Pool`'s own
//! `mio` worker threads push `PoolEvent`s that get translated to
//! `core::EngineMsg::RelayConnected`/`RelayDisconnected`/`RelayFrame` on
//! that same inbox.
//!
//! `Handle` is the cheap, `Clone + Send` value the app holds: it sends
//! command `EngineMsg`s in and registers row/receipt sinks. The threading
//! is entirely interior — the app never sees `mio`, never sees a
//! `PoolEvent`, never adopts a runtime (§2, P1).
//!
//! Step 0: empty shells only. C (depends on B + A2) wires the
//! `PoolEvent` <-> `EngineMsg` translation and reconnection replay.

/// One dedicated engine OS thread (§2 position 2).
///
/// Step 0: empty shell. C fills in `spawn`, the blocking `mpsc` recv loop,
/// and the `nmp_transport::PoolEvent` translation.
pub struct EngineThread;

/// The cheap, `Clone + Send` app-facing handle. Exactly four verbs plus
/// sink registration (ledger #2/#3 preserved at the top edge — plan §5
/// test 14 grep-guards this structural property):
///
/// - `subscribe(LiveQuery, RowSink) -> HandleId`
/// - `unsubscribe(HandleId)`
/// - `set_active_pubkey(Option<PublicKey>)`
/// - `publish(WriteIntent, ReceiptSink) -> Receipt`
///
/// No `relays:` parameter, no open-REQ method — C must not add one.
///
/// Step 0: empty shell. C fills in the four verbs, each just sending an
/// `EngineMsg` onto the owning `EngineThread`'s inbox.
#[derive(Clone)]
pub struct Handle;
