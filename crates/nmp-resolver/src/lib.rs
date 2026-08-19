//! `nmp-resolver` — the graph engine, atom refcounting, identity register,
//! and metrics that resolve the reactive filter-binding grammar
//! (`nmp-grammar`) into abstract demand-set deltas against a borrowed
//! `RedbStore` (`nmp-store`). See the M1 grammar-engine plan (git history)
//! §2.3-§6 for the full spec this crate implements.
//!
//! Module layout:
//! - `types` — the small shared vocabulary (`NodeId`, `Element`,
//!   `FieldSlot`, `ParentLink`).
//! - `eval` — pure leaf computations (projection, set algebra, identity
//!   resolution, element merging). No kind-literal branching anywhere in
//!   this crate — see the M1 plan's kill guard (§3.3 step 2, §6).
//! - `graph` — the node graph: data + pure, store-independent algorithm
//!   (atom computation, wide-query-filter computation, structural
//!   traversal).
//! - `engine` — `Engine`: construction, incremental
//!   recompute, identity re-root, subscribe/unsubscribe, and the public
//!   API surface (`HandleId`, `QueryHandle`, `Metrics`,
//!   `GraphSnapshot`).

mod eval;
mod graph;
mod types;

mod engine;

pub use engine::{
    CommittedCurrentRow, CommittedMutationResult, CommittedRowChanges, Engine, GraphNodeInfo,
    GraphSnapshot, HandleId, LocalAcceptResult, Metrics, QueryHandle, RelayIngestError,
    RelayIngestResult, ResolutionNodeKind, ResolutionNodeSnapshot, ResolvedValue,
    SemanticInstallResult, SubscribeOutcome,
};
