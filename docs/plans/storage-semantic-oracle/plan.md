# Backend-independent storage semantic oracle

## Summary

Define one deterministic, backend-neutral semantic trace and crash matrix before NMP evaluates another persistent engine.

## Detailed Plan

## Outcome

Create one backend-independent semantic oracle that becomes the admission test for future storage work. It must prove event-plane and publishing/control semantics, including recovery, without enabling a new production backend.

## Implementation

1. Add a test-support oracle module in `nmp-store` with a deterministic fixture builder and operation trace. Keep its vocabulary private or test-feature-gated so it cannot become a third public app noun.
2. Normalize observable state into canonical records: full event identity/content, provenance and local ownership, signature state, replacement winners, tombstone effects, expiry visibility, coverage claims, open intents, retained receipts, route revisions, attempts, lanes, deadlines, and terminal results. Serialize the normalization deterministically and compute a content digest.
3. Define ordered query probes for global recency, author/kind, addressable identity, tag filters, provenance-constrained pages, and cursor boundaries. Compare exact ordered IDs and normalized rows.
4. Drive a deterministic scenario covering duplicate ingest and provenance growth; replaceable/addressable conflict and stale refusal; deletion and deletion-before-target tombstones; expiry; coverage and coverage-safe GC; accepted pending writes; promotion to signed bytes; explicit cancellation and compensation; route revisions; transient retry; handoff; ACK/reject publication facts; and retained receipt reattachment. Compare normalized state, ordered query probes, and digest after every operation.
5. Run the same uninterrupted scenario against MemoryStore and fresh/reopened Redb. Store expected checkpoints independently of physical keys or tables.
6. Add crash qualification hooks. For each durable mutation, declare the allowed recovered checkpoint. Redb crash injection/reopen tests must prove exact normalized state, query order, and digest—not just row counts. Model split-authority seams as control commit, event projection, projection acknowledgement, and service barrier; verify control-first replay is deterministic and idempotent before query/transport service. Do not change the production persistence protocol in this PR.
7. Correct checked-in issue #629 benchmark text: use `process-accounted write bytes`; describe hidden/internal Fjall seams as benchmark-only; state that row-count reopen checks do not prove exact recovery. Add a durable oracle design note tied to VISION section 3.1, the design record, #698, and #699. Record unimplemented candidate adapters as open work, not as implied qualification.

## Validation

- Run the oracle after every operation against MemoryStore and Redb.
- Run Redb crash/reopen cases and assert exact normalized checkpoint plus digest.
- Run `cargo test -p nmp-store`.
- Run architecture gate scripts and formatting/lint checks relevant to touched files.
- Review the diff for accidental production backend selection or new app-facing types.

## Rollout and rollback

The change is test/documentation infrastructure and does not alter production backend selection. If the harness exposes an existing redb semantic mismatch, keep the failing case as evidence and split any behavior change into a separately tracked issue rather than weakening the oracle. Rollback is removal of the qualification harness; no data migration exists.

## Risks

- An oracle built from backend implementation details would bless the current layout. Mitigation: normalize only public semantic observations and deterministic full content.
- A single happy-path trace could miss combinatorial conflicts. Mitigation: explicit operation checkpoints and focused crash seams, with room for generated traces later.
- MemoryStore recovery differs by design. Mitigation: use it for semantic expectations and Redb for persistence/reopen proof.
- Control-plane breadth can make snapshots unstable. Mitigation: canonical sorting and explicit exclusion of wall-clock/process-local fields.

## Follow-up sequence

Only after this PR lands: qualify Fjall through the complete semantic path with compaction fully settled and accounted; qualify LMDB through a packed backend-native Nostr layout; evaluate SQLite and browser persistence as a separate portability track. None of those results alone selects a winner.

## Rule And ADR Check

- AGENTS issue-first discipline is satisfied by epic #698 and child #699 before repository changes.
- VISION section 3.1 and the design record already define Accepted as a logical crash-consistent boundary and allow a control-first replay journal across stores.
- The Noun Gate remains satisfied because the oracle is test/qualification infrastructure, not a new app-facing noun.
- The destructive and lifecycle gates are unchanged; the harness exercises existing typed operations rather than adding bypasses.
- Redb remains the only production baseline, and no candidate backend becomes reachable in production.
- Per-crate tests, architecture gates, and falsifier-honesty checks will run before publication.

## Possible Rule Or ADR Loosening

- Retire the historical wording that required event and publishing state to share one physical transaction. Preserve the stronger logical invariant through explicit authority, ordering, idempotency, replay, and startup reconciliation.
- Do not require native and WASM targets to use the same physical engine; require them to pass the same semantic contract.

## Possible Rule Tightening

- A backend may not be called production-qualified from physical row counts, a basic reopen check, or write-byte measurements alone.
- Future backend evaluations must pass the shared semantic trace and crash matrix and publish completed maintenance/compaction accounting before a production migration is proposed.

## Alternatives Considered

- Port Fjall first: rejected because it would duplicate or discover semantics inside a candidate implementation and risk fitting the contract to one engine.
- Treat MemoryStore tests as the complete oracle: rejected because volatile behavior cannot establish restart, journal replay, exact receipt recovery, or crash ordering.
- Compare physical table row counts after reopen: rejected because equal counts can hide wrong winners, provenance, tombstones, receipt states, order, or bytes.
- Require one database engine everywhere: rejected because portability is a logical-contract problem and browser persistence has different constraints.

## Certainty

90 percent.

## Decision

ready

## Hosted Artifacts

- Plan page: Generated after publishing.

- TTS audio: https://blossom.primal.net/5d3cbfee5a65980eeb8873530e67dbba3e06fa0ef04475987ac6b3dcfc26f69c.mp3
