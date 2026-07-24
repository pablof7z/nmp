//! `nmp-resolver` — the graph engine, atom refcounting, identity register,
//! and metrics that resolve the reactive filter-binding grammar
//! (`nmp-grammar`) into abstract demand-set deltas over an `EventStore`
//! (`nmp-store`). See `docs/plans/M1-grammar-engine-plan.md` §2.3-§6 for the
//! full spec this crate implements; `src/testkit.rs` is the scripted
//! fake-relay harness, and `tests/` holds the M1 contract tests (the pass
//! criteria).
//!
//! Module layout:
//! - `types` — the small shared vocabulary (`NodeId`, `Element`,
//!   `FieldSlot`, `ParentLink`).
//! - `eval` — pure leaf computations (projection, set algebra, identity
//!   resolution, element merging). No kind-literal branching anywhere in
//!   this crate (excluding `testkit`/`tests`) — see the M1 plan's kill
//!   guard (§3.3 step 2, §6) and contract test 10.
//! - `graph` — the node graph: data + pure, store-independent algorithm
//!   (atom computation, wide-query-filter computation, structural
//!   traversal).
//! - `engine` — `Engine<S: EventStore>`: construction, incremental
//!   recompute, identity re-root, subscribe/unsubscribe, and the public
//!   API surface (`LiveQuery`, `HandleId`, `QueryHandle`, `Metrics`,
//!   `GraphSnapshot`).

mod eval;
mod graph;
mod types;

mod engine;
#[cfg(feature = "bench-instrumentation")]
pub mod ingest_attribution;
pub mod testkit;

pub use engine::{
    CommittedCurrentRow, CommittedMutationResult, CommittedRowChanges, Engine, GraphNodeInfo,
    GraphSnapshot, HandleId, LiveQuery, LocalAcceptResult, Metrics, QueryHandle, RelayIngestResult,
    ResolutionNodeKind, ResolutionNodeSnapshot, ResolvedValue,
};
