//! [`DemandOp`] and [`DemandDelta`] — the abstract demand-set delta the
//! resolver emits: sets of concrete resolved filters to open/close.

use crate::concrete::ConcreteFilter;

/// A single demand-set operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DemandOp {
    /// Withdraw demand for this concrete filter.
    Close(ConcreteFilter),
    /// Assert demand for this concrete filter.
    Open(ConcreteFilter),
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

    /// The concrete filters opened by this delta, in order.
    pub fn opened(&self) -> Vec<&ConcreteFilter> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DemandOp::Open(f) => Some(f),
                DemandOp::Close(_) => None,
            })
            .collect()
    }

    /// The concrete filters closed by this delta, in order.
    pub fn closed(&self) -> Vec<&ConcreteFilter> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                DemandOp::Close(f) => Some(f),
                DemandOp::Open(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn cf(kind: u16) -> ConcreteFilter {
        ConcreteFilter {
            kinds: Some(BTreeSet::from([kind])),
            ..ConcreteFilter::default()
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
                DemandOp::Close(cf(1)),
                DemandOp::Close(cf(2)),
                DemandOp::Open(cf(3)),
            ],
        };
        assert!(!d.is_empty());
        assert_eq!(d.closed(), vec![&cf(1), &cf(2)]);
        assert_eq!(d.opened(), vec![&cf(3)]);
    }
}
