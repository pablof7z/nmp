//! `nmp-transport` — the generational WebSocket `Pool`: the async/threaded
//! edge (M3 plan §1, §2, §3.2). HARVEST target: the old repo's
//! `mio`-driven worker-thread pool (`crates/nmp-network` in
//! `nostr-multi-platform`) — see plan §4 "Transport pool" for exactly what
//! is re-justified vs. dropped.
//!
//! This crate depends on NO other NMP crate (§1: "pure edges → parallel
//! builders, isolated tests"). It never imposes a runtime on its caller —
//! `mio` worker threads are interior to [`pool::Pool`]; nothing here is
//! `async fn`, and there is no tokio anywhere in this stack (D8).
//!
//! Step 0 scaffold only: this module declares the wire/handle/health
//! vocabulary from §3.2. A2 fills in `Pool`'s connect/send/close/backoff/
//! keepalive/reconnect-replay behavior.

mod handle;
mod health;
mod pool;

pub use handle::RelayHandle;
pub use health::{ConnState, RelayHealth};
pub use pool::{
    DisconnectReason, Pool, PoolConfig, PoolEvent, PoolEventSink, RelayFrame, WireFrame,
};
