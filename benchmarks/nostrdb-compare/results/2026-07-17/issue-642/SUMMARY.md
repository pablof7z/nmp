# Issue #642 — redo-journaled Redb index result

This checkpoint tests whether derived indexes can leave NMP's immediate
durability barrier without weakening recovery. The candidate uses two linked
transactions per bounded batch:

1. canonical/provenance facts plus a complete index redo payload commit with
   Redb `Durability::Immediate`;
2. ordered/tag/cardinality indexes materialize atomically, delete that redo
   row, and commit with `Durability::None`.

Live visibility would occur only after phase 2. Reopen replays any redo row
that outlived its non-sync materialization before reads are served.

## Result

Eleven clean-tree paired repetitions processed the representative 100k-event
corpus in alternating order with 4,096-event batches. Every run reopened with
the exact expected cardinality for all twelve logical keyspaces.

| Paired/median metric | Atomic baseline | Redo candidate | Difference |
| --- | ---: | ---: | ---: |
| Paired throughput ratio | — | 0.905x | Redo 9.5% slower |
| Independent median throughput | 58,875/s | 53,199/s | directional only |
| Median host writes | 342.6 MB | 394.7 MB | Redo 15.2% higher |
| Median peak RSS | 426.7 MB | 455.1 MB | Redo 6.6% higher |
| Median stored bytes | 73.1 MB | 73.1 MB | equal |
| Redo payload | — | 42.2 MB | additional durable input |

The candidate won two paired repetitions and lost nine. Redb timing remained
variable, so the paired ratio is the decision authority.

The non-sync index commit itself is cheap at a 76 ms median. The immediate
facts-plus-redo commit still costs 1.026 seconds, versus 0.961 seconds for the
baseline's single atomic commit. Serializing and durably writing 42.2 MB of
redo evidence replaces the avoided synchronous index pages with another large
copy-on-write workload. Total writes rise instead of falling.

## Recovery falsifiers

Forced worker exits cover all protocol boundaries:

- after the immediate facts-plus-redo commit;
- after the non-sync index materialization; and
- after a later immediate commit makes prior non-sync state durable.

In every case reopen plus redo recovery yields exactly one canonical row, one
derived index row, and no remaining redo row. The codec rejects truncation and
trailing bytes. Graceful matrix runs need no replay because Redb closes their
latest non-sync state consistently.

## Decision

Close this protocol negative and do not carry it into the governed production
path. It proves that recoverable non-sync indexes can be made unambiguous, but
the durable redo payload makes representative ingest 9.5% slower and increases
host writes. That fails the issue's requirement to plausibly move #627 by a
multiplier before accepting the added two-phase recovery machinery.

The evidence narrows the constraint further: moving unchanged index bytes into
a redo log does not change Redb's copy-on-write cost model. A viable design must
avoid durably writing the same logical index information twice and materially
reduce the physical bytes dirtied per accepted event.
