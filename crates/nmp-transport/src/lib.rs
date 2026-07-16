//! `nmp-transport` — the generational WebSocket `Pool`: the async/threaded
//! edge (M3 plan §1, §2, §3.2). HARVEST target: the old repo's
//! `mio`-driven worker-thread pool (`crates/nmp-network` in
//! `nostr-multi-platform`) — see `pool`/`pool::worker`/`pool::inner`'s doc
//! comments for exactly what is re-justified vs. dropped, and this crate's
//! builder report for the harvest-vs-rewrite summary.
//!
//! This crate depends on NO other NMP crate (§1: "pure edges → parallel
//! builders, isolated tests"). It never imposes a runtime on its caller —
//! `mio` worker threads are interior to [`pool::Pool`]; nothing here is
//! `async fn`, and there is no tokio anywhere in this stack (D8).
//!
//! ## Generation safety (the load-bearing invariant)
//!
//! Every [`RelayHandle`] is `(slot, generation)`. A frame or command issued
//! against a handle whose generation has been superseded — by ANY
//! reconnect, not only an explicit close+reopen — is a structural no-op:
//! [`Pool::send`]/[`Pool::close`]/[`Pool::health`] all return a
//! stale-rejection sentinel (`false`/`None`), and an inbound frame tagged
//! with a superseded generation is dropped by the pool's translator before
//! it is ever handed to [`PoolEventSink`]. See `pool::worker`'s
//! `pack_generation` doc for the exact scheme.
//!
//! ## Surface
//!
//! [`Pool::ensure_open`] / [`Pool::send`] / [`Pool::close`] /
//! [`Pool::set_reconnect_preamble`] / [`Pool::health`] / [`Pool::shutdown`].
//! Push-model only: there is no "send to all" — the caller iterates its own
//! routing plan and issues one `send` per (relay, current handle).

mod admission;
mod backoff;
mod handle;
mod health;
mod keepalive;
mod pool;

pub use admission::{
    classify_ip, classify_relay_host, normalize_bare_host, relay_host_key, RelayHostClass,
};
pub use handle::RelayHandle;
pub use health::{ConnState, RelayHealth};
pub use pool::{
    AttemptCorrelation, DisconnectReason, DurableSendOutcome, EphemeralSendOutcome,
    EphemeralSendStart, HandoffResult, Pool, PoolBuildError, PoolConfig, PoolEvent, PoolEventSink,
    RelayFrame, RelayOpenError, RelaySessionKey, ThreadRole, ThreadSpawnError, WireFrame,
    DEFAULT_MAX_RELAYS, DEFAULT_VERIFIER_WORKERS,
};
