# Issue #639 — sorted Redb index mutation result

This checkpoint asks whether applying ordered/tag index mutations in B-tree key
order reduces Redb's dominant durable-commit cost. Both layouts stage the same
bounded transaction's index records after all non-index records. The control
preserves event-arrival order; the candidate sorts by logical index and key.
Canonical rows, provenance, relay rows, cardinality, values, transaction size,
and durability are identical.

## Result

Eleven clean-tree paired repetitions processed the representative 100k-event
corpus in alternating order with 4,096-event transactions. The load-bearing
paired medians are:

| Metric | Sorted versus arrival order |
| --- | ---: |
| Effective throughput, including staging | 0.8% slower |
| Database wall time | 0.3% lower |
| Commit time | 0.3% higher |
| Host writes | effectively identical |
| Stored bytes | identical |
| Peak RSS | effectively identical |

Sorted order won five throughput pairs and lost six. Redb timing varied widely,
but the paired median is stable at no material change. Comparing independent
backend medians would incorrectly report a large win because slow host periods
did not affect the two sequential children equally; that number is rejected.

Median staging time was 147 ms for key sorting versus 81 ms for the control's
stable partition. The additional sort work is included in effective throughput.
Even before that cost, paired database wall and commit ratios remain within
0.3% of the control. Sorting therefore neither reduces dirty-page writes nor
changes durable commit behavior.

Every run reopened with the exact expected cardinality for all twelve logical
keyspaces. The Redb abrupt-exit falsifier from #637 covers the same physical
tables and atomic boundary: a committed row survives and a staged uncommitted
row does not.

## Decision

Close key-order staging negative. Do not add bounded mutation buffers or
production complexity for a measured throughput regression and unchanged
commit/write behavior.

Together with #637, the evidence says Redb's constraint is not avoidable by
reordering or regrouping the same index records. A viable next storage candidate
must reduce the number/size of durable index mutations or use a different page
update model without Fjall's governed-path memory and disk regressions.
