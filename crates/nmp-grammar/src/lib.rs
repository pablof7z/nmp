//! `nmp-grammar` — the reactive filter-binding grammar's value types
//! (VISION §2 P2): `Filter`, `Binding`, `Selector`, `ConcreteFilter`,
//! `DemandOp`/`DemandDelta`, and canonical descriptor hashing.
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
mod selector;

pub use binding::{Binding, Derived, Filter, SetAlgebra, SetOp};
pub use concrete::{ConcreteFilter, ContextualAtom, DescriptorHash};
pub use demand::{DemandDelta, DemandOp};
pub use descriptor::{AccessContext, CacheMode, Demand, SourceAuthority};
pub use indexed_tag_name::IndexedTagName;
pub use selector::{IdentityField, Selector};
