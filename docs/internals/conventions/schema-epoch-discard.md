---
title: A schema epoch bump discards the whole store
category: conventions
slug: schema-epoch-discard
status: policy
date: 2026-07-30
owns:
  - what a persistent schema epoch bump requires from consumers
  - which durable facts are lost when a non-current store is discarded
  - why NMP never drains or decodes an old outbox before recreation
related:
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/design/durable-write-signing-and-retry.md
issues:
  - 867
  - 1017
---

# A schema epoch bump discards the whole store

Pablo (repository owner, 2026-07-29):

> wipe the store, the backlog is worthless if it's stale

This is the no-backwards-compatibility rule applied to durable bytes. It is a
deliberate data-discard policy, not an accidental consequence of the current
Redb implementation.

---

## 1. The rule — POLICY

NMP supports exactly one persistent schema epoch: the current
`SCHEMA_VERSION`. A nonempty store without exactly that marker is refused at
open. To run the current NMP build, the consumer must close every owner,
discard the non-current store, and create a fresh one.

The whole database is one epoch. Stored events, provenance, coverage,
tombstones, accepted write obligations, pending rows, receipts, correlation
tokens, route revisions, attempts, and retry state share one transaction
boundary. A schema bump therefore discards the whole store; none of those
domains is independently carried across it.

The refusal never:

- decodes a retired row or table;
- migrates, adopts, relabels, or aliases old bytes;
- drains the old outbox into a fresh store;
- copies or re-accepts unpublished obligations; or
- silently resets the store during open.

Even an outbox-only recovery door would be a per-epoch decoder. It is excluded
for the same reason as a full migration.

## 2. What discard costs — POLICY

The relay-backed read cache can be reacquired from relays after recreation.
The durable write outbox cannot:

- accepted but unpublished writes are deleted;
- their pending rows and receipts are deleted;
- their correlation tokens, route revisions, attempt evidence, and retry
  state are deleted.

That loss is permanent. An operator must not be told merely to "wipe the
cache," because the file contains publication obligations as well as cached
relay events.

The outbox is not drained first. A write that remained unpublished long enough
to cross a schema epoch is stale by policy: publishing its old `created_at` and
content later can produce an out-of-order artifact that no longer represents
the user's present intent. Dropping that obligation is safer than reviving it
through a compatibility decoder. If the user still wants the action, the app
must create a new present-tense intent through the current surface.

## 3. The operator contract — BUILT

`RedbStore::open` acquires exclusive ownership before inspecting the epoch and
returns `RedbStoreOpenError::UnsupportedSchema` before exposing a store or
mutating an application durable fact. Its displayed error says both:

1. discard and recreate the store to continue; and
2. the relay-backed read cache can be reacquired, but the durable write outbox
   is permanently lost.

Current-epoch corruption remains a different `Database(Corrupted(...))`
failure. An operator must never mistake damaged current bytes for a safely
discardable old epoch.

That separation reaches the app, not just the store (#920). `Engine::new`
returns `EngineError::StoreUnsupportedSchema { path, expected, found }` —
mirrored through UniFFI, Swift, and Kotlin — so a consumer branches on a type
instead of matching the refusal's prose. Every other open refusal, damaged
bytes and refused locks alike, stays `EngineError::StoreOpenFailed`, which
carries the positive claim that discarding the store is not its recovery.
`found` is `None` when the store carries no marker this build can read, which
includes a marker written at a superseded epoch's address; it means "not this
epoch", never "no data".

The open refusal does not delete anything. Destruction is a separate,
deliberate consumer action after every live owner is closed — NMP states the
epoch fact and never decides the discard.

## 4. Falsifiers — BUILT

`crates/nmp-store/src/redb_store/tests.rs` proves the contract without retaining
knowledge of any retired schema:

- a marker-less nonempty database and markers immediately below and above
  `SCHEMA_VERSION` all reach the one typed refusal without application-data
  mutation;
- the reachable refusal text names the reacquirable relay cache and every
  durable-outbox loss category above; and
- damaged current-epoch rows remain typed corruption, never schema discard.

There is deliberately no old-schema fixture, old table inventory, or
pre-current decoder in those tests. Negative executable awareness of one
retired layout would still keep that layout alive.

The app-facing half is proved separately, because a refusal that is typed at
the store and collapsed at the facade is not reachable by the consumer this
policy addresses. `crates/nmp/src/engine.rs` and
`crates/nmp-ffi/src/facade.rs` each drive one nonempty store whose marker this
build cannot read and one file of damaged bytes, and assert the two arrive as
different variants and that only the epoch one renders "discard and recreate";
Swift and Kotlin assert the same branch without reading any message.
