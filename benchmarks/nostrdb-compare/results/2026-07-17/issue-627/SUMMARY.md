# Issue #627 storage amplification ceiling

This run asks whether a different physical engine can remove the dominant
representative-event write cost without weakening NMP's durability contract.
It compares byte-identical prepared writes across Redb, LMDB, and Fjall, then
measures the real relay-to-live-query pipeline with Redb and the volatile
`MemoryStore` semantic oracle.

The decision is deliberately narrower than a migration decision. Prepared
engine work can select a candidate, but only a complete `EventStore`
implementation can measure NMP query behavior and declare an end-to-end win.

## Measurement contract

- Harness commit: `84794829b62be634dbcf80a15803895b4abd6194`
  (`git_dirty=false` in both engine matrices).
- Host: `kind2`, Linux 6.1.0-42-amd64, Intel Core Ultra 7 265, ext4 on
  `/dev/md1` for databases and tmpfs for corpus input.
- Representative source: #620's checked private-free shape corpus, expanded
  deterministically to 100,000 signed events. JSONL BLAKE3:
  `5eb48a3d4e4d051619c9f6656eed697dd1c1bf8eb210de5f9211ec7c0178ad36`.
- Equivalent engines receive the same pre-encoded keys and values for all 12
  required event, provenance, ordered-index, tag-index, and cardinality
  keyspaces. Preparation is outside the timed region.
- Fjall uses a serialized cross-keyspace transaction and explicit
  `PersistMode::SyncAll` on every commit. Its faster default buffer durability
  is not used.
- Every matrix cell reopens and proves the exact expected cardinality of all 12
  keyspaces. The representative matrix alternates order for five repetitions;
  the uniform context matrix uses three.
- The crash probe commits one row synchronously, stages a second uncommitted
  row, exits through `_exit(73)`, and requires the committed row alone after
  reopen.

## Equivalent prepared-engine result

The production path already assembles transactions up to 4,096 events (#623),
so the 4,096 row is the load-bearing comparison.

### Representative corpus, five-run median

| Engine | Events/s median (range) | vs Redb | Commit ms | Process writes | Logical DB | Exact reopen |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Redb | 46,206 (18,473–49,301) | 1.00x | 1,149 | 720 MB | 269 MB | 5/5 |
| LMDB | 58,445 (51,995–63,068) | 1.26x | 1,086 | 768 MB | 242 MB | 5/5 |
| Fjall | 58,917 (50,203–60,209) | **1.28x** | 1,317 | **126 MB** | **126 MB** | 5/5 |

Fjall reduces observed initial-load process writes **5.72x** and logical size
53.3% versus Redb. Its throughput is also much less variable in this run, but
the five-run median gain is 1.28x, not the issue's 2x production gate.

At 128 events/transaction Fjall reaches 9,046/s versus Redb's 5,886/s
(1.54x) and writes 130 MB versus 2,895 MB (22.2x lower). This confirms that
Fjall avoids Redb's small-transaction write cliff, but NMP's production
assembler has already moved beyond that cliff.

### Uniform context, three-run median at batch 4,096

| Engine | Events/s | vs Redb | Process writes | Logical DB |
| --- | ---: | ---: | ---: | ---: |
| Redb | 36,031 | 1.00x | 343 MB | 269 MB |
| LMDB | 38,457 | 1.07x | 350 MB | 145 MB |
| Fjall | 67,760 | 1.88x | 95 MB | 95 MB |

The event distribution materially changes the engine ratio. Uniform results
must not be substituted for the representative decision corpus.

## Real pipeline ceiling

Five paired 100,000-event production probes reverse Redb/Memory order every
repetition. Both cross the real websocket, JSON parse, signature verification,
resolver, governed semantics, and bounded live-query path. `MemoryStore` is an
upper-bound control, not a persistence candidate: it does no durable write or
reopen and retains substantially more resident owned data.

| Backend | Events/s median (range) | vs Redb | Ingest ms | Peak RSS |
| --- | ---: | ---: | ---: | ---: |
| Redb | 18,988 (13,187–19,426) | 1.00x | 5,266 | 178 MB |
| Memory | 45,689 (44,445–46,744) | **2.41x** | 2,189 | 643 MB |

The median-throughput Redb run spends 3,441 ms of 5,266 ms in the governed
store transaction path:

| Store phase | Time | Share of store time |
| --- | ---: | ---: |
| commit | 1,557 ms | 45.3% |
| flush | 502 ms | 14.6% |
| ordered/tag indexes | 660 ms | 19.2% |
| canonical rows | 301 ms | 8.8% |
| governed apply residual | 392 ms | 11.4% |
| encoding | 26 ms | 0.8% |

Commit plus flush is 59.9% of store time. Durable storage is the massive
constraint; parse (413 ms) and signature verification (1,062 ms) are the next
material pre-store costs. The 2.41x storage-free ceiling also means the epic's
full 2x target leaves almost no room for a merely incremental store win.

## Correctness and portability boundary

Redb, LMDB, and Fjall all pass the committed/uncommitted forced-exit probe.
Fjall also passes exact reopen in every one of its 16 checked matrix cells.

The matrix's `reopen_ns` includes a full cardinality validation. Fjall's
`Readable::len` scans a keyspace, while Redb exposes O(1) table length, so that
field is not a pure database-open comparison and is not treated as a startup
regression here.

Fjall is pure safe Rust and removes the C/native build liability of LMDB, but
pure Rust does not itself establish browser/WASM support. Its browser/WASM,
Apple/Android packaging, pure-open latency, real NMP selective-query latency,
steady-state compaction, deletion/update amplification, and one-million-event
behavior remain unproven.

## Decision

Keep Fjall as the leading physical-store candidate; do not dismiss its 5.72x
write reduction. Do not migrate NMP on this prepared-engine result alone.

The next honest slice is a benchmark-gated Fjall `EventStore` integration (or
shared governance/physical-KV seam) that preserves every atomic event,
deletion, expiry, replacement, coverage, outbox, receipt, and lane invariant.
It must measure the real production pipeline, selective queries, pure open,
steady-state compaction, one-million exact reopen, and mobile/WASM consequences.

In parallel, #612 still needs a second gain in parse/verification/event
materialization: a naive serial attribution of the 1.28x prepared-store gain
projects only about 1.16x whole-pipeline throughput. That estimate selects work;
it is not a claimed production result.

## Compact ordered-ID selection experiment

Follow-up commit `cc900e4127a9041cd5e1595371a1e7e2c04e535c` tests the most
direct nostrdb-inspired reduction that NMP had not already adopted. NMP already
decodes 64-character hexadecimal event IDs, pubkeys, and identity-shaped tag
atoms to 32 raw bytes. The remaining difference is that NMP repeats the full
32-byte event ID in every ordered index key to make equal-timestamp rows unique
and preserve exact `created_at DESC, event_id ASC` pagination.

The selection layout retains only the first 8 ordered ID bytes, removing 24
bytes from each global, author, kind, author-kind, and tag mutation. It rejects
distinct compact-key collisions instead of silently overwriting them. This is
not a production format: an exact collision sidecar would be mandatory if the
ceiling justified implementation.

Fifteen fresh-process pairs alternated the production-width and compact layouts
over the representative 100,000-event corpus at batch 4,096. All 30 runs
reopened every expected logical row. Paired medians:

| Metric | Compact / baseline | Result |
| --- | ---: | ---: |
| throughput | 1.030x | 3.0% faster |
| commit wall | 0.924x | 7.6% lower |
| process writes | 0.865x | 13.5% lower |
| Redb stored bytes | 0.875x | 12.5% lower |
| prepared key/value bytes | 89.25 / 101.93 MB | 12.4% lower |

The compact layout won 10 of 15 throughput pairs, but host variance was much
larger than the median wall-time effect. The deterministic write and size
reductions are real; the sustained throughput multiplier is not.

Decision: do not build the exact collision sidecar on this ceiling. Shortening
the repeated ordered ID materially reduces bytes but does not approach #627's
2x production gate, and the benchmark excludes the extra collision metadata,
lookups, migration, and query logic a correct implementation would require.
This rules out straightforward ordered-ID truncation as the massive lever; it
does not relax exact tie ordering or discard the observed write reduction.

## Sampled planner-cardinality implementation

The representative decomposition exposed a different high-cardinality write:
exact query-planner counts create one durable Redb row for nearly every unique
author, author-kind pair, and tag value. Those counts choose a physical index;
they do not determine query membership, ordering, replacement, deletion, or
coverage semantics.

The implemented sidecar counts a keyed uniform one-in-sixteen event sample.
The 32-byte per-store key prevents a relay from cheaply grinding event IDs to
control planner estimates. Insert and removal use the same stable predicate;
the sampled delta commits with the exact event indexes. Sidecar version 2
rebuilds from ordered-index ID suffixes under the persisted key before queries,
and a healthy reopen remains read-only. Query execution and post-filtering are
unchanged and exact.

Fifteen alternating fresh-process pairs on the 100,000-event representative
corpus measured the full equivalent index set:

| Metric | Sampled / exact | Result |
| --- | ---: | ---: |
| throughput | 1.492x | 49.2% faster |
| commit wall | 0.693x | 30.7% lower |
| process writes | 0.750x | 25.0% lower |
| Redb stored bytes | 0.885x | 11.5% lower |
| peak RSS | 0.948x | 5.2% lower |

That reduced writer is the physical ceiling. A separate fifteen-pair run
through the real governed `RedbStore::insert_batch` path measured a paired
median import ratio of 0.734: **26.6% faster** while producing the same 100,000
canonical rows in every run.

Five paired query runs then exercised complete kind, author, author-kind, tag,
two-tag, 43-author union, and bounded kind/tag shapes. Every exact and sampled
run returned identical row counts. The worst paired-median p95 ratio was 1.017
for the 42,270-row complete-kind scan; every other shape was neutral or faster.
This passes #627's no-more-than-10% selective-query regression gate without
claiming that sampling makes reads faster.

Decision: ship sampled planner cardinalities as a measured incremental win.
They do not satisfy #627's 2x ingest gate alone, so #627 remains open; they do
remove the first large, semantically unnecessary mutation class without
changing any app-visible result or weakening event-store atomicity.
