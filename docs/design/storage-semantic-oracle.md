# Redb semantic and recovery qualification

Issue #699 established the semantic trace. Issue #1427 removed the duplicate
store implementation: NMP and its test suites now use Redb for the complete
durable-store contract.

## Authority and recovery contract

The logical contract has two authority domains:

- The event plane owns canonical event bytes, indexes, provenance,
  replacement/address winners, tombstones, expiry, suppression, and coverage.
  A committed mutation cannot leave those facts disagreeing.
- The publishing/control plane owns frozen or signed bytes, intent and receipt
  identity, route revisions, attempts, retries, cancellation, terminal facts,
  and any projection journal. A committed mutation cannot lose an accepted
  obligation or invent publication.

Redb commits both domains through one physical transaction.

## What the trace proves

The test-only trace runs one exact-byte fixture through a temporary Redb store's
typed semantic operations, without reading Redb tables or file bytes. Its
independently spelled operations and expected outcomes cover:

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

## Status

No candidate implements NMP's complete durable-store contract in the current
architecture.
