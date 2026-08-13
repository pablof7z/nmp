# Redb semantic and recovery qualification

Issue #699 established the semantic trace. Issue #1427 removed the duplicate
store implementation: Redb is now the only complete `EventStore` backend.

## Authority and recovery contract

The logical contract has two authority domains:

- The event plane owns canonical event bytes, indexes, provenance,
  replacement/address winners, tombstones, expiry, suppression, and coverage.
  A committed mutation cannot leave those facts disagreeing.
- The publishing/control plane owns frozen or signed bytes, intent and receipt
  identity, route revisions, attempts, retries, cancellation, terminal facts,
  and any projection journal. A committed mutation cannot lose an accepted
  obligation or invent publication.

Redb commits both domains through one physical transaction. Any future proposal
to replace it must satisfy all of these rules before it becomes an
implementation:

1. Commit the complete control obligation before projecting a local pending
   event or performing signing/transport side effects.
2. Give each projection a durable unique identity and total order.
3. Make projection deterministic and idempotent, including replacement,
   deletion, expiry, and local ownership effects.
4. Reconcile every unapplied projection before ordinary query or transport
   service starts.
5. Persist event facts before advancing coverage claims. Coverage authority is
   owned by the exact request/session/wire FIFO whose EVENT facts were
   observed; an event-transaction failure revokes only the owners the wire
   evidence can name. Every claim earned by one request commits as one atomic
   batch. Temporary underclaim may refetch; overclaim or a visible prefix of a
   multi-claim request can hide absent data and is forbidden.
6. Treat an operation as successful only when the authority that owns its fact
   has durably committed it. Recovery may expose a declared pre-operation,
   post-operation, or reconciled state—never an unclassified mixture.

Native and browser/WASM proposals may require different engines. Engine
identity alone would not be portability evidence.

## What the trace proves

The test-only trace runs one exact-byte fixture through `EventStore`, without
reading Redb tables or file bytes. Its independently spelled operations and
expected outcomes cover:

- duplicate ingest and relay-provenance growth;
- replaceable and addressable conflicts;
- deletion and deletion-before-target tombstones;
- expiry, request-level multi-claim coverage, and coverage-safe GC;
- pending acceptance, signature promotion, and pre-signature cancellation;
- durable routes, a transient retry, transport handoff, ACK, retained receipt,
  and terminal obligation cleanup.

After every operation the trace normalizes complete query-visible rows and
provenance, ordered query projections, coverage, receipts, routes, attempts,
lanes, deadlines, and expiry state. It computes a BLAKE3 digest of that
canonical representation and checks the exact expected outcome.

The harness closes and reopens the database after every operation and
compares both the semantic snapshot and recovery-only journal state before and
after reopen. Every existing SIGABRT failpoint additionally computes a
semantic recovery digest and proves it is stable across a second
reopen; each failpoint's focused test remains responsible for classifying the
result as the allowed pre- or post-operation state. The request-coverage crash
falsifier uses the real event-before/after-commit and coverage-before/after-
commit seams with a two-claim batch. Reopen may expose `{no fact, no claim}`,
`{fact, no claim}`, or `{fact, all claims}`—never `{no fact, claim}` or a
one-of-two prefix. This combination replaces row-count recovery claims with
content, atomicity, and ordering evidence.

## Future replacement sequence

No candidate is an `EventStore` implementation in the current architecture.
Before a future replacement decision:

1. Evaluate Fjall through the complete semantic path, then fully settle and
   account for compaction and other deferred maintenance.
2. Evaluate LMDB with a packed, backend-native Nostr layout rather than the
   historical Redb-shaped twelve-keyspace schema.
3. Evaluate SQLite and browser persistence as a distinct portability track.

No single benchmark decides a winner. A replacement proposal requires the
semantic trace, completed maintenance accounting, performance evidence on the
full governed path, and an explicit production rollout decision.
