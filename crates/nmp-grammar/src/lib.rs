//! `nmp-grammar` — the reactive filter-binding grammar's value types
//! (VISION §2 P2): `Filter`, `Binding`, `Selector`, `ConcreteFilter`,
//! `DemandOp`/`DemandDelta`, canonical descriptor hashing, the pure NIP-19/
//! NIP-21 locator codec, and pure relay-host classification.
//!
//! This crate holds **value types only** — no graph, no engine, no event
//! matching. `nmp-resolver` owns evaluating a `Filter` (expanding its
//! `Binding`s) down to `ConcreteFilter`s and diffing demand; this crate only
//! defines what those values *are* and how a `ConcreteFilter` lowers to
//! `nostr::Filter` (`to_nostr`) and hashes canonically (`hash`).
//!
//! Event <-> filter matching is deliberately NOT reimplemented here: the
//! lowered `nostr::Filter` is matched against events via
//! `nostr::Filter::match_event` (memory rule: use rust-nostr, not scratch
//! logic).

mod binding;
mod concrete;
mod demand;
mod descriptor;
mod indexed_tag_name;
mod live_query;
mod nip19;
mod replaceable_materializer;
mod row;
mod selector;
mod tagging;
mod text;
mod write;

pub use binding::{Binding, Derived, Filter, SetAlgebra, SetOp};
pub use concrete::{
    fold_byte, fold_context, ConcreteFilter, ContextualAtom, DescriptorHash, RoutingEvidence,
    RoutingEvidenceKind,
};
pub use demand::{DemandDelta, DemandOp};
pub use descriptor::{
    AccessContext, CacheMode, Demand, DemandError, Freshness, RelaySessionKey, SourceAuthority,
};
pub use indexed_tag_name::IndexedTagName;
pub use live_query::{LiveQuery, LiveQueryError};
pub use nip19::{decode as decode_nostr_entity, NostrEntity, NostrEntityError};
pub use replaceable_materializer::{
    RegisteredReplaceableMaterializer, ReplaceableMaterializer, ReplaceableMaterializerOperation,
    ReplaceableMaterializerRefusal, ReplaceableMaterializerRegistration,
    ReplaceableMaterializerSpec,
};
pub use row::{first_verified_source, sentinel_signature, Row, RowDelta, RowSignature};
pub use selector::{IdentityField, Selector};
pub use tagging::{
    entity_rows, event_parent_rows, event_root_rows, reply_to, Modifiers, Pointer, RootScope,
    TagOptions, TagRows, Tagged, ThreadPosition, COMMENT_KIND, TEXT_NOTE_KIND,
};
pub use text::{At, InterpolatedContent, Mention};
pub use write::{
    EventBuilder, Identity, ReplaceableOperation, ReplaceableOperationError,
    ReplaceableOperationStart, WriteIntent, WritePayload, WriteRouting,
};
