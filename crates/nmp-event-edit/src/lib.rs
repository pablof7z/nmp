//! Protocol-neutral, loss-preserving structural event edits.
//!
//! Capability crates own typed values, identity, normalization, legacy
//! decoding, current encoding, conflict meaning, and which operations exist.
//! They compile those decisions into one closed, versioned [`EventEditPlan`].
//! Replaying a plan needs only raw document data: no callback, module registry,
//! protocol number, or global kind ownership enters this crate.
//!
//! A plan is the capability-normalized representation of one semantic
//! operation. This crate deliberately accepts one plan at a time and owns no
//! receipt or operation history. #1408 retains the compact ordered plans that
//! still contribute; #841 applies that set to the selected base. #1406 owns the
//! long-sequence cost proof for those responsibilities.
//!
//! Tag edits work over raw cells and preserve every row they do not select.
//! JSON-object edits replace byte spans rather than serializing a parsed value,
//! preserving untouched whitespace, key spelling, order, duplicate fields, and
//! number lexemes. Public and private tag partitions are applied independently;
//! this crate never claims a total order across them.

mod json;
mod plan;
mod tags;

pub use json::{JsonApplyError, JsonEditOutcome, JsonSpanPatch};
pub use plan::{
    Boundary, EventEditPlan, EventEditV1, JsonFieldEdit, JsonMissing, Occurrences, Partition,
    PartitionedTagEdit, PlanError, TagEdit, TagInsertion, TagItemPattern, TagItemSelector,
    TagRowPattern,
};
pub use tags::{PartitionedTagEditOutcome, TagApplyError, TagEditOutcome};

#[cfg(any(test, feature = "bench-instrumentation"))]
pub use json::JsonApplyMetrics;
#[cfg(any(test, feature = "bench-instrumentation"))]
pub use tags::TagApplyMetrics;
