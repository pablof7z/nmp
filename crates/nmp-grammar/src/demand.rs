//! [`DemandOp`] and [`DemandDelta`] — the abstract demand-set delta the
//! resolver emits: sets of resolved atoms to open/close.

use crate::concrete::ContextualAtom;

/// A single demand-set operation. Carries a full [`ContextualAtom`] (#106,
/// was a bare `ConcreteFilter`) — the delta reflects the SAME identity the
/// engine's atom-refcount table keys on, so a caller inspecting `opened()`/
/// `closed()` sees an atom's true `source`/`access`, not just its selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandOp {
    /// Withdraw demand for this atom.
    Close(ContextualAtom),
    /// Assert demand for this atom.
    Open(ContextualAtom),
}

/// A demand-set delta.
///
/// INVARIANT: all `Close` ops precede all `Open` ops; `Close` ops are
/// emitted in reverse-of-open order (teardown-before-activate at every
/// node). Producing this ordering is the resolver's job (M1 plan §3.3/§3.6);
/// this type just carries the ordered sequence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DemandDelta {
    /// The ordered operations: closes first (reverse-of-open order), then
    /// opens.
    pub ops: Vec<DemandOp>,
}

impl DemandDelta {
    /// An empty delta (no demand change).
    pub fn empty() -> Self {
        Self::default()
    }

    /// True iff this delta has no operations.
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// The atoms opened by this delta, in order.
    pub fn opened(&self) -> Vec<&ContextualAtom> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DemandOp::Open(a) => Some(a),
                DemandOp::Close(_) => None,
            })
            .collect()
    }

    /// The atoms closed by this delta, in order.
    pub fn closed(&self) -> Vec<&ContextualAtom> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DemandOp::Close(a) => Some(a),
                DemandOp::Open(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concrete::ConcreteFilter;
    use crate::descriptor::{AccessContext, SourceAuthority};
    use std::collections::BTreeSet;

    fn atom(kind: u16) -> ContextualAtom {
        ContextualAtom {
            filter: ConcreteFilter {
                kinds: Some(BTreeSet::from([kind])),
                ..ConcreteFilter::default()
            },
            source: SourceAuthority::Public,
            access: AccessContext::Public,
            routing_evidence: BTreeSet::new(),
        }
    }

    #[test]
    fn empty_delta_reports_empty_and_no_ops() {
        let d = DemandDelta::empty();
        assert!(d.is_empty());
        assert!(d.opened().is_empty());
        assert!(d.closed().is_empty());
    }

    #[test]
    fn opened_and_closed_partition_the_ops_preserving_order() {
        let d = DemandDelta {
            ops: vec![
                DemandOp::Close(atom(1)),
                DemandOp::Close(atom(2)),
                DemandOp::Open(atom(3)),
            ],
        };
        assert!(!d.is_empty());
        assert_eq!(d.closed(), vec![&atom(1), &atom(2)]);
        assert_eq!(d.opened(), vec![&atom(3)]);
    }
}
