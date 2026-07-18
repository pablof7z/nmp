# Storage semantic oracle

Issue #699 establishes the admission test for storage backends. It does not
select a database or change the production backend. Redb remains the baseline.

## Authority and recovery contract

The logical contract has two authority domains:

- The event plane owns canonical event bytes, indexes, provenance,
  replacement/address winners, tombstones, expiry, suppression, and coverage.
  A committed mutation cannot leave those facts disagreeing.
- The publishing/control plane owns frozen or signed bytes, intent and receipt
  identity, route revisions, attempts, retries, cancellation, terminal facts,
  and any projection journal. A committed mutation cannot lose an accepted
  obligation or invent publication.

Those domains may share one physical transaction, as Redb currently does, or
use separate stores. A split implementation must satisfy all of these rules:

1. Commit the complete control obligation before projecting a local pending
   event or performing signing/transport side effects.
2. Give each projection a durable unique identity and total order.
3. Make projection deterministic and idempotent, including replacement,
   deletion, expiry, and local ownership effects.
4. Reconcile every unapplied projection before ordinary query or transport
   service starts.
5. Persist event facts before advancing coverage claims. Temporary underclaim
   may refetch; overclaim can hide absent data and is forbidden.
6. Treat an operation as successful only when the authority that owns its fact
   has durably committed it. Recovery may expose a declared pre-operation,
   post-operation, or reconciled state—never an unclassified mixture.

Native and browser/WASM implementations may use different engines. They must
pass the same semantic contract; engine identity is not portability evidence.

## What the oracle compares

The test-only oracle runs one exact-byte fixture through `EventStore`, without
reading backend tables or file bytes. It covers:

- duplicate ingest and relay-provenance growth;
- replaceable and addressable conflicts;
- deletion and deletion-before-target tombstones;
- expiry and coverage-safe GC;
- pending acceptance, signature promotion, and pre-signature cancellation;
- durable routes, a transient retry, transport handoff, ACK, retained receipt,
  and terminal obligation cleanup.

After every operation it normalizes complete query-visible rows and
provenance, ordered query projections, coverage, receipts, routes, attempts,
lanes, deadlines, and expiry state. It computes a BLAKE3 digest of that
canonical representation. MemoryStore and Redb must match exactly.

The Redb harness closes and reopens the database after every operation and
compares both the semantic snapshot and recovery-only journal state before and
after reopen. Every existing SIGABRT failpoint additionally computes a
backend-independent recovery digest and proves it is stable across a second
reopen; each failpoint's focused test remains responsible for classifying the
result as the allowed pre- or post-operation state. This combination replaces
row-count recovery claims with content and ordering evidence.

## Qualification sequence

Only after the oracle is present:

1. Evaluate Fjall through the complete semantic path, then fully settle and
   account for compaction and other deferred maintenance.
2. Evaluate LMDB with a packed, backend-native Nostr layout rather than the
   historical Redb-shaped twelve-keyspace schema.
3. Evaluate SQLite and browser persistence as a distinct portability track.

No single benchmark decides a winner. A migration proposal requires semantic
oracle results, completed maintenance accounting, performance evidence on the
full governed path, and an explicit production rollout decision.
