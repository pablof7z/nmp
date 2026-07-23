//! `nmp-engine` — the M3 runtime and THE sync/async seam (plan §2, §3.4).
//!
//! - [`core`] — `EngineCore`: the PURE synchronous reducer. No I/O, no
//!   threads, no imposed runtime. Its whole surface is
//!   `handle(EngineMsg) -> Vec<Effect>` / `tick(Timestamp) -> Vec<Effect>`.
//!   This is what keeps the whole engine headlessly testable (§2 position 1).
//! - [`runtime`] — the async edge: `EngineThread` (one dedicated OS thread,
//!   blocking `mpsc` recv loop, D8) + `Handle` (the cheap `Clone + Send`
//!   value the app holds).
//! - [`outbox`] — the write-intent/receipt plane (durability class, typed
//!   routing, the receipt stream).
//! - [`negentropy`] — the prober FSM + `ProbedRelay` capability token +
//!   `Reconciler` (a MODULE, not a crate — §1: reducer-coupled).
//!
//! Dependency direction: everything in the M1/M2/M3 crate graph flows into
//! this crate; nothing depends on it (§1).
//!
//! Step 0 scaffold only — see each module's doc comment for what it
//! declares vs. defers to the A/B/C/D/E builders (plan §6).

pub mod core;
#[cfg(feature = "bench-instrumentation")]
pub mod ingest_attribution;
pub mod negentropy;
pub mod outbox;
pub mod relay_information;
pub mod runtime;

/// Monotonic count of real NMP-owned OS threads spawned this process (#680
/// falsifier instrumentation). Covers the executor's transient-adapter threads
/// and the engine runtime/bridge threads; the thread-scaling falsifier asserts
/// the delta across opening many observations is 0.
pub use nmp_executor::nmp_threads_spawned;

pub use runtime::{
    AddAuthPolicyError, AuthPolicy, AuthPolicyDecision, AuthPolicyError, AuthPolicyOp,
    AuthPolicyPendingSender, AuthPolicyRegistration, AuthPolicyRequest, AuthPolicyResolveError,
    PendingAuthPolicyOp, RuntimeConfig, DEFAULT_MAX_AUTH_CAPABILITIES,
};
