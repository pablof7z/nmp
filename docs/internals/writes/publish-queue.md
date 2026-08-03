---
title: Publish queue storage
category: writes
slug: publish-queue
status: built
date: 2026-07-30
audience: llms
scope: binary persistence for accepted write obligations and per-relay delivery
owns:
  - execution-side delivery vocabulary
  - the publish_queue_* key and value model
  - schema and codec version refusal
  - relay-surrogate allocation and recovery
  - crash and performance qualification for the representation cut
related:
  - docs/design/durable-write-signing-and-retry.md
  - docs/internals/writes/relay-worker-demand.md
  - docs/internals/conventions/schema-epoch-discard.md
issues:
  - https://github.com/pablof7z/nmp/issues/1027
  - https://github.com/pablof7z/nmp/issues/1026
  - https://github.com/pablof7z/nmp/issues/771
  - https://github.com/pablof7z/nmp/issues/889
  - https://github.com/pablof7z/nmp/issues/1134
---

# Publish queue storage

NMP uses two different concepts that used to share the word “outbox”:

- **outbox routing** selects relays, including NIP-65 author write relays;
- **publish queue** executes an accepted write against the selected relays.

The routing term remains correct. The execution module, store doors, types,
diagnostics, fixtures, and current documentation use `delivery`: for example
`PublishQueueLane`, `PublishQueueAttempt`, `PublishQueueDeadline`, `PublishQueueReceipt`, and
`recover_publish_queue`. There is no compatibility alias for the retired
execution-side spelling.

The authority for this cut is the repository owner’s wording:

> create gh issues with these plans, including the binary stuff -- no need to migrate from the current stuff -- start fresh, we'll wipe the existing outbox lanes.

> it would be a good opportunity to rename this stuff from outbox to something else so it doesn't conflict with "outbox routing" which is a completely different thing, right?

## Fresh-store cut

The whole Redb schema epoch is version 12. Publish queue has a second,
explicit codec marker with version 1 in `publish_queue_meta_v1`. A new database
creates only `publish_queue_*` execution tables. It never opens, drains,
transforms, dual-writes, or deletes a legacy execution table.

A nonempty pre-cut database therefore reaches the existing typed
`RedbStoreOpenError::UnsupportedSchema` refusal before a store handle is
returned and before any byte is changed. Reset is a separate, explicit,
offline destructive operator action. Opening an old file cannot look like an
empty delivery journal, and opening it does not automatically wipe anything.

This is one schema epoch rather than independently migratable delivery tables:
canonical events, coverage, accepted obligations, receipts, correlations,
routes, lanes, attempts, details, and deadlines share transactions. Carrying a
partial compatibility decoder would make that atomic model dishonest.

## Physical model

Every ordering-sensitive key is fixed width and big-endian:

| Fact | Key |
|---|---|
| intent, displaced row, receipt, kind:5 claim | `u64 intent/receipt` |
| relay dictionary row | `u32 relay_id` |
| lane | `intent:u64 \| relay_id:u32` |
| attempt and attempt detail | `intent:u64 \| relay_id:u32 \| ordinal:u64` |
| route revision | `intent:u64 \| ordinal:u64` |
| ordered deadline | `at:u64 \| intent:u64 \| relay_id:u32` |
| deadline cleanup index | `intent:u64 \| at:u64 \| relay_id:u32` |
| correlation and address suppression | bounded bytes, with binary values |
| id suppression | raw `event_id:[u8;32] \| pubkey:[u8;32]` |

The namespace consists of:

`publish_queue_intents_v1`, `publish_queue_displaced_v1`,
`publish_queue_receipts_v1`, `publish_queue_correlations_v1`,
`publish_queue_route_revisions_v1`, `publish_queue_lanes_v1`,
`publish_queue_attempts_v1`, `publish_queue_attempt_details_v1`,
`publish_queue_deadlines_v1`, `publish_queue_deadlines_by_intent_v1`,
`publish_queue_relays_v1`, `publish_queue_relay_ids_v1`,
`publish_queue_kind5_claims_v1`, `publish_queue_suppress_by_id_v1`,
`publish_queue_suppress_by_addr_v1`, and `publish_queue_meta_v1`.

Values use an explicit eight-byte envelope: four ASCII magic bytes, one codec
version byte, and three zero reserved bytes. Integers are big-endian; variants
have explicit tags independent of Rust discriminants; fields have deterministic
order. Lengths are `u32` and checked before allocation. Current bounds are
4,096 bytes per relay URL or reason, 65,536 bytes per general text field,
16 MiB per embedded event, 4,096 relays per route revision, and 65,536
suppression claimants. Truncation, bad magic, unknown version, nonzero reserved
bytes, unknown tags, invalid UTF-8, noncanonical relays, overlong fields, and
trailing bytes all return typed persistence evidence. Durable-delivery
persistence uses no `serde_json`, `bincode`, host-width integer, host
endianness, or backend-specific enum layout.

Golden tests independently spell the bytes for a relay dictionary value and
the prefix-sortable key families. The explanations identify every byte rather
than accepting a regenerated digest as authority.

## Relay identity: measured surrogate choice

Delivery uses a stable `u32` relay surrogate. The forward dictionary stores
`relay_id -> versioned canonical RelayUrl`; the reverse dictionary stores
`canonical RelayUrl bytes -> relay_id`. Route revisions, lanes, attempts,
details, and deadline keys then carry four bytes rather than repeating a
variable URL.

The allocator, forward row, reverse row, route revision, and all references
created by that operation commit in one transaction. Keys increase
monotonically and are never reused. Reads validate the forward/reverse
bijection. A process-local cache parses a dictionary value once per distinct
relay and returns clones thereafter; the cache is not authority.

The release fixture below uses only four distinct relays across 1,000 lanes.
Against canonical length-prefixed bytes repeated in the legacy string/JSON
shape, the surrogate candidate reduced median allocated database bytes from
4,276,224 to 1,564,672 (63.4%). That result justifies the dictionary for this
workload; it is not a universal claim that a surrogate wins every cardinality
distribution.

## Authority and transaction boundaries

`EventStore` remains the only policy-independent door. The engine decides
routes, retry causes, eligibility, and terminal outcomes; the store atomically
records the selected fact. In particular:

- acceptance commits the canonical pending row, intent, receipt, correlation,
  displacement/suppression effects, and allocators together;
- signature promotion or pre-signature compensation updates the canonical row,
  intent, receipt, and suppression/displacement state together;
- route revision and any new relay dictionary rows commit together;
- lane cursor, immutable attempt, additive detail, and both deadline indexes
  change in the same transition transaction;
- terminal close removes bounded open-work rows only after every persisted lane
  is terminal, while retained receipt and attempt evidence survive.

The cut deliberately does **not** persist reducer `route_complete`. Under
[#1134](https://github.com/pablof7z/nmp/issues/1134), an Auto-routed obligation
recomputes current-process completeness after restart. Durable strategy, route
revisions, lanes, attempts, and receipt identity survive; session-scoped
completeness does not.

## Semantic and crash qualification

The backend-independent oracle compares MemoryStore and RedbStore after every
operation. Redb is closed and reopened at each checkpoint, and both normalized
state and its BLAKE3 digest must remain exact. The trace covers acceptance,
signing, cancellation, failure compensation, replaceable supersession,
correlation lookup, a three-relay route, retry, interruption and resume,
`OutcomeUnknown`, `GaveUp`, relay rejection, ACK, and terminal close. It
compares events, ordered queries, routes, ordinals, attempt details, lanes,
deadlines, receipts, correlations, and open-work recovery rather than row
counts.

Process-death tests also hash the recovered semantic projection. Existing
transaction failpoints now exercise binary delivery rows, including acceptance,
promotion/compensation, route revision, lane/detail/deadline transitions,
receipt/correlation changes, and close. Each crash has only the door’s exact
pre-state or exact post-state as an allowed result. Corrupt, missing,
contradictory, truncated, unknown-version, noncanonical, and overlong records
fail closed without a partial lane set or fabricated receipt.

No `route_complete` field or table exists.

## Isolated performance evidence

`crates/nmp-store/examples/publish_queue_recovery_bench.rs` runs population and
recovery in separate release processes with a counting allocator and Linux
process-I/O accounting. The comparison used exact base
`625e976d670fe035efa57e42debb332943902c98` and the candidate implementation.
The baseline source differs only by the mechanical old/new Rust identifiers
needed to compile the same benchmark against each API.

The representative fixture is 250 open signed intents, four shared relays per
intent, one route revision and one transient attempt per lane: 1,000 lanes and
exactly 4,000 commits in both builds. Population ran in three alternating
base/candidate pairs. Recovery ran six alternating fresh-process pairs over
settled databases. Every run recovered 572,124 normalized semantic bytes and
the same scheduler-effect digest:
`9d9aefb66fe1d2efea6fdc85d8f7730fca92f0a7501c6f4af810b654155e9127`.

| Median measure | legacy string/JSON | binary delivery v1 | change |
|---|---:|---:|---:|
| Logical database bytes after reopen | 5,189,632 | 1,990,656 | -61.6% |
| Allocated database bytes after reopen | 4,276,224 | 1,564,672 | -63.4% |
| Process write bytes during population | 192,118,784 | 162,492,416 | -15.4% |
| Population wall time | 18.822 s | 15.805 s | -16.0% |
| Population allocation operations | 1,173,037 | 997,645 | -14.9% |
| Population allocated bytes | 353,920,845 | 299,677,352 | -15.3% |
| Full reopen/recovery wall time | 54.341 ms | 43.658 ms | -19.7% |
| Recovery allocation operations | 132,606 | 44,326 | -66.6% |
| Recovery allocated bytes | 15,081,272 | 5,803,830 | -61.5% |
| Recovery process write bytes | 4,096 | 4,096 | unchanged |
| Commits represented | 4,000 | 4,000 | unchanged |

Sampling profiles could not be collected on the evidence host because
`perf_event_paranoid=3` denies unprivileged performance events; no percentage
is invented to fill that gap. Attribution is instead bounded to facts the
fixture and source can prove:

- the base delivery recovery path has 17 production `serde_json::from_str`
  sites across intents, receipts, lanes, deadlines, routes, attempts, details,
  and suppression metadata;
- its `&str` keys make Redb validate UTF-8 during key handling, and relay URLs
  are reconstructed from repeated JSON values;
- active `delivery.rs`, `publish_queue_ops.rs`, and `publish_queue_codec.rs` have zero
  `serde_json` use; their ordered keys are fixed byte arrays;
- the candidate parses four relay dictionary values once per fresh recovery
  process and caches those four canonical identities while recovering 1,000
  lanes;
- the measured 66.6% allocation-count and 61.5% allocated-byte reductions
  bound the removed decode/materialization work, but do not assign all wall
  time to a single function.

This is physical-representation evidence for Redb, not backend qualification or
a database winner. Binary rows lower the cost of recovery and remaining scans;
they do not make a repeated scan disappear. The reducer-owned lane projection
remains the mechanism that removes ordinary dispatch scans, while later
scheduler work owns any further read elimination or batching.
