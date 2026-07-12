//! Synthetic facade used to exercise compiler-produced rustdoc JSON.

pub use first_alias::{generic_transform as transform, PublicAlias as GroupedAlias};
pub use first_alias::{CycleA as GroupedCycle, Outer as GroupedOuter};
pub use first_alias::Outer as DuplicateOuter;
