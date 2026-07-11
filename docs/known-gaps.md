# Known gaps & deferred follow-ups

Honest running list of things built-but-incomplete or deliberately deferred, so nothing hides. Each says who flagged it, why it's deferred, and when it must be closed. This is a truth-anchor companion to the bug-class ledger.

## Promoted v2 contract gaps - next work frame

The July 11 architecture promotion intentionally corrected several assumptions
after the original milestones. These are agreed target contracts, not claims
about current code:

- **Demand is still filter-shaped.** The supported descriptor does not yet carry
  `selection + source authority + access context` end to end. Hashing,
  coalescing, routing, evidence, FFI, Swift, and Kotlin must project the same
  semantic descriptor. See `docs/design/query-demand-and-evidence.md`.
- **Current `Coverage` over-interprets relay evidence.** `Unknown` versus
  aggregate `CompleteUpTo` and the builder's authoritative-empty language must
  become rows plus compact per-current-plan acquisition facts. Diagnostics keep
  exact per-relay EOSE/watermark/AUTH/error evidence; no public global
  completeness or `syncHealth` state remains.
- **Durable `Accepted` is not crash-safe acceptance of a canonical pending
  row.** The intent journal, receipt, frozen unsigned body, stable pending row,
  displaced replaceable state, and initial retry state are not yet one atomic
  persistence boundary. Restart, cancellation, signature promotion, and
  reattachment proofs are required.
- **Signer selection is globally coupled today.** The target default is the
  signer registered for `$currentPubkey`, with an optional per-write identity
  override pinned at acceptance. Missing capability must persist as
  `AwaitingSigner(pubkey)`. Standard platform secure signer providers and
  reattachment are unbuilt.
- **Durable logical retry is unbuilt.** The outbox does not yet persist exact
  per-`(intent, relay)` attempt bytes, ordinal, outcome, and next eligibility.
  The deadline driver must grow into the one scheduler for expiry, liveness,
  signer operations, and retry without adding polling or transport-owned
  durable buffering.
- **Protocol-module composition is unbuilt.** The existing ownership design
  incorrectly makes kind ownership gate all route authority. Modules must claim
  only exact NIP-defined schemas while typed contextual operations may add their
  own tags and route facts to immutable foreign-owned drafts. No kind:1-first
  core catalog is part of the target.
- **Boundedness is only partial.** Swift newest-frame buffering, indexed queries,
  and router caps exist, but graph, derived-set, wire, relay, result, receipt,
  ingestion, and scheduler bounds do not yet share an explicit shortfall
  contract. Silent first-N behavior is forbidden.
- **Destructive trust-domain reset is missing as a defined contract.** One
  engine is one shared cache across accounts. A mutually-untrusted-user logout
  must atomically clear cached events, pending writes, receipts,
  coverage/evidence, and related local state.
- **Public syntax remains provisional.** Any change now requires failure
  evidence, Rust/persistence/diagnostics/FFI/Swift/Kotlin impact review, updated
  falsifiers, human signoff, and removal of the superseded path. The repository
  has not yet enforced this promotion protocol across every public projection.

## Load-bearing for M5 (the falsifier app) — must close before M5 claims pass

- **~~`RelayDirectory` has no reactive update path~~ CLOSED (self-bootstrapping outbox).** `nmp_router::LiveDirectory` is a live, updatable `RelayDirectory` (write relays start empty, fed at runtime via `RelayDirectory::ingest_write_relays`); `nmp_engine::core::EngineCore::sync_discovery` watches active content demand for authors whose write relays are still unknown and opens an internal kind:10002 discovery subscription against the configured indexers for exactly them (reusing the ordinary resolver subscribe/unsubscribe machinery, not a parallel subscription system) -- when that kind:10002 lands, the winning event is re-read from the store and fed into the directory, and the very same recompile re-routes that author's content atoms to their real write relay. `nmp-demo`'s two-phase `bootstrap.rs`/`BootstrapDirectory` are deleted; the CLI now configures only two indexer relays and gets real notes with the engine doing discovery. `nmp-ffi`'s `NmpEngineConfig`/Swift `NMPConfig` lost the `write_relays`/`writeRelays` field for the same reason -- an app supplies indexers only. Headless proof: `nmp-engine/tests/self_bootstrap_outbox.rs`. Live proof: `nmp-demo` against real relays, and `Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`.

- **~~Publish payload is unsigned-only across FFI (M4)~~ CLOSED (#32).** `FfiWritePayload` now has `Unsigned`/`Signed` variants (mirroring `nmp_engine::outbox::WritePayload`); a caller holding an already-signed event (external signer / NIP-46 bunker / verbatim republish) submits `.Signed` and the engine publishes it verbatim -- no re-sign, no tag mutation, no id recomputation. `convert::signed_event_from_ffi` verifies the reconstructed event (`nostr::Event::verify`) at the FFI boundary before it ever reaches the engine; a malformed or non-verifying event returns a typed `FfiError::InvalidSignature`/`InvalidSignedEvent`, never publishes. Swift `WritePayload` mirrors both cases. Falsifier: `crates/nmp-ffi/src/convert.rs`'s `ffi_publishes_presigned_event_verbatim`/`ffi_presigned_never_resigned`/`ffi_rejects_malformed_signed_event`/`ffi_rejects_signed_event_with_unparseable_signature`, plus `Packages/NMP/Tests/NMPTests/FilterBuilderTests.swift`'s `testSignedWriteIntentConversion`.

- **~~kind:10002 discovery over-fetch (7112 events for a 39-author set)~~ CLOSED (churn fix).** The M5 dogfooding session's diagnostics screen showed `wire_sub_count: 1` / `authors_served: 39` / a SINGLE, correctly-scoped `{"authors":[...39...],"kinds":[10002]}` filter against `purplepag.es` -- yet `events kind:10,002 = 7112` had been received. Root-caused (NOT the "unscoped/wildcard filter" theory the wire evidence had already ruled out): `nmp_engine::core::EngineCore::sync_discovery` tore down and reopened the internal kind:10002 discovery subscription as a fresh overwriting `Req` every single time an author's write relays resolved (each resolution shrank the filter's `authors` set by exactly one). To a NIP-01-compliant relay, an overwriting `Req` on an already-open sub-id is indistinguishable from a brand-new subscription: it replies with a full EOSE replay of every currently-matching stored event. Resolving N authors one at a time this way sums to a triangular N+(N-1)+...+1 redelivery, not O(N) -- confirmed exactly by a headless falsifier (`nmp-engine/tests/discovery_churn.rs`): 39 authors resolving one-by-one pre-fix produced 39 separate `Req` ops and 819 total author-resends (~triangular ceiling 780); post-fix, 1 `Req` op and 40 resends. **Fix:** `sync_discovery` is now widen-only -- a newly-needed author still widens the subscription (unchanged), but an author leaving `needed` no longer tears anything down; it's simply left in the filter (widen-safe: a wider author set only ever matches MORE, never fewer, the same proof obligation `nmp_router::coalesce`'s `AuthorUnion` rule already carries) until `needed` goes fully empty, at which point the subscription actually closes. Live-relay re-verification (`nmp-demo` against `purplepag.es` + `relay.primal.net`): 196+45=241 total kind:10002 events for a 193-author resolved set (~1.2x, not ~182x). Also fixed as part of the same investigation: `nmp_router::facts::LiveDirectory::ingest_write_relays` removed an author's directory entry entirely when their kind:10002 declared zero write relays, instead of recording "known, zero relays" as the trait's own contract requires (`RelayDirectory::ingest_write_relays`'s doc) -- and `nmp_router::facts::DiscoveryKinds`'s default now covers kind:0/3 plus the WHOLE NIP-01 replaceable range (10000..=19999), not just the four kinds NMP happened to read (owner-affirmed semantics).

- **Unbounded historical replay can peg the main thread (M5 dogfooding finding), bound across two halves (#17).** `apps/Falsifier` (the M5 SwiftUI app) reproducibly saturates a simulator's main thread at ~97-98% CPU for 1-2 minutes, twice: (1) whenever a query without a `limit` (e.g. the app's `FeedFilters.followsRelayLists()`, `kinds:[10002]`) is freshly `observe`d, and (2) whenever `observeDiagnostics()` is first iterated. `sample` on the running process shows sustained top-of-stack time in `nmp_store::redb_store::RedbStore::EventStore::query` plus `serde_json`/schnorr-signature JSON parsing, not idle waiting -- real, repeated work, not a hang (it does eventually finish and CPU returns to 0%).
  - **Swift-delivery half CLOSED.** `NMPQuery`/`NMPDiagnostics` used to re-deliver the full accumulated snapshot on every single delta (no batching/coalescing), so an ordinary app iterating `for await batch in query` with ordinary SwiftUI `@State` writes got many consecutive full re-renders, starving the run loop. **Fix:** `Packages/NMP/Sources/NMP/FrameCoalescer.swift` -- `RowBridge`/`DiagnosticsBridge` now coalesce delivery to at most one snapshot per ~16ms (~60Hz) window, always the LATEST accumulated state (no delta is ever dropped from the final state, only intermediate *deliveries* are), plus `.bufferingNewest(1)` on both `AsyncStream`s so a consumer slower than the coalescing cadence still can't accumulate a growing backlog. Live-relay-verified (`Packages/NMP/Tests/NMPTests/LiveRelayTests.swift`, real replay against `purplepag.es`/`relay.primal.net`) plus a dedicated unit falsifier (`FrameCoalescerTests.swift`) proving a 200-push tight-loop burst collapses into a handful of deliveries with the final delivered value exactly equal to the last pushed value.
  - **Rust query-cost half CLOSED (#38); per-event refresh cost now bounded — on-device re-verification pending.** `nmp-store`'s `RedbStore::query` used to decode every row's JSON with no index narrowing (the dominant `sample` cost). **Fix (#38):** two persistent redb secondary indexes (`BY_AUTHOR`/`BY_KIND`) maintained in lockstep through the one centralized `remove_row_in_txn`/insert path (so they cannot drift across supersession/kind:5/expiry/gc); `query` now does bounded index range-scans for id/author/kind/address filters and only JSON-decodes the narrowed candidate set (falsifier: an author-filtered query over 1 target + 200 noise rows decodes exactly 1). The other named cost — `crates/nmp-engine/src/core/mod.rs` refreshing all handles after every ingested event — is unchanged, but each refresh is now a *cheap indexed* query rather than a full-table scan, so the O(events × handles) blow-up is bounded. **Honest status:** the root cause is fixed and the Swift-delivery half caps re-render frequency, but the ~97% CPU jank has NOT been re-measured on device with all three fixes (Swift coalescing + Rust index + churn) live — verify the running result on the Falsifier before declaring the M5 jank gone. Screenshots: `docs/screenshots/m5-06-diagnostics-loading-jank.jpg`, `m5-07-diagnostics-steady-state.jpg`.

## Real but non-blocking for the falsifier (feeds, not DMs)

- **~~DM inbox routing incorrect (M3-D)~~ CLOSED (#19).** `WriteRouting::ToInboxes` used to fall back to the union of recipients' *write*+extra relays because `RelayDirectory` had no read/inbox accessor. **Fix (#19):** `RelayDirectory` grew `read_relays` (lane `Nip65Read`) + `ingest_read_relays`; `LiveDirectory` stores both read- and write-marked kind:10002 entries from the same winning event (`parse_nip65_read_relays`, NIP-65 unmarked = both); and `EngineCore::resolve_routes`' `ToInboxes` branch now consumes `read_relays` ONLY — a recipient with no known inbox relay (unknown or write-only) fails the whole intent CLOSED with a typed `Failed` before any `PublishEvent`, never falling back to write relays. Falsifiers: `core_headless.rs` `to_inboxes_*` (read-only routes, write-only + unknown fail closed) and `core::nip65_read_write_split_tests` (unmarked=both, read/write-marker split, one-winner-both-sets).

- **Decrypt-result feedback path missing (M3-C, plan §8 item 2).** `Effect::RequestDecrypt` is an explicit no-op; there is no `EngineMsg` to feed a decrypt result back into ingest. Needed for reading NIP-17 DMs / private NIP-51 items (ledger #12 encrypted-content path). Deferred with E/negentropy still open; not on the falsifier's feed path.

- **Reconnect loses negentropy-first temporarily (M3-E).** On reconnect, subs previously routed negentropy-first are replayed as plain REQ (safe, correct — just less efficient) until the next real demand change re-routes them. A "reroute negentropy-first on reconnect" refinement was deferred. Perf, not correctness.

- **~~No time driver for liveness/timeout sweeps (M3-E)~~ CLOSED (#39 via PR #42).** The engine loop's `cmd_rx.recv()` is now `recv_timeout(next_deadline − now)`, armed from `EngineCore::next_deadline()` (min over the store expiration index + neg-session liveness deadlines): zero new threads, wakes exactly at the next real deadline, blocks forever on `recv()` when none exist, re-arms every iteration (an ingest introducing an earlier deadline re-arms naturally — no interrupt machinery). NIP-40 expiry fires event-driven through the same driver. Review caught + fixed a ~1s 100% CPU busy-spin (the neg-liveness sweep threshold `> N` was misaligned with the armed deadline `started_at + N`; now `now >= started_at + N`, so the tick that fires the deadline also clears it); regression test (`neg_liveness_deadline_does_not_busy_spin`) hand-verified failing pre-fix at ~986ms CPU, passing post-fix. D8-clean, no polling.

## Design-level (validated from external feedback — see docs/reviews/2026-07-11-external-feedback-triage.md)

- **Supersession retraction blindness - base design complete, pending-write section superseded (build pending).** The resolver still needs the symmetric negative-delta lane described in `docs/design/retraction-and-negative-deltas.md`: store commits return inserted and removed rows, and resolver invalidation consumes both. That mechanism remains correct for supersession, kind:5, NIP-40 expiry, and explicit pre-signature cancellation. The document's optimistic-write details are no longer canonical: a durable accepted row has typed `Pending(intentId) | Signed(signature)` state; missing signer waits indefinitely; only cancellation or terminal **pre-signature** failure retracts and compensates a displaced replaceable; relay rejection after signing changes receipt evidence only. `docs/design/durable-write-signing-and-retry.md` owns that correction. Kind:5 tombstone retention remains an owner decision. Not yet built.

- **~~Four bounded correctness fixes from the external-feedback triage~~ LANDED (merge `9220f65`).** (1) Signature-verification gate at the network layer (`nmp-transport` frame seam) — kind-independent, verify-once per event id (redelivery string-compares the cached sig, no re-schnorr), invalid sig → drop + `RelayHealth::invalid_signature_count`; cache reads never re-verified. Makes ledger #5 honest. (2) FFI no longer panics on malformed `Literal` hex (typed error) and no longer silently drops malformed tags (`tags_from_ffi` returns `Result` — NMP can't sign a different event than the app composed). (3) `DescriptorHash`/`CoverageKey` widened FNV-64 → BLAKE3 256-bit (a network-controlled, durable-and-refcount key must be collision-resistant; a forged collision there would forge a `CompleteUpTo`). (4) `coalesce` never merges limited filters (relay-side truncation under-fetch), and a known-zero-write-relay author stops perpetual discovery.

## Security hardening deferred

- **Secret zeroization and platform signer-provider boundary are not complete.** `nmp-signer` currently holds `nostr::Keys` without the old repo's zeroize/raw-bytes hardening. The promoted contract also requires the durable Rust event/outbox store to persist obligations rather than secrets and the Swift/Kotlin SDKs to offer standard secure-storage-backed signer providers. Both need falsification before v2 ships. Owner: security/signing workstream.

## Process / tooling

- **CI runs on push but is not proven to gate a PR** (no branch protection configured). The workflow exists (`.github/workflows/ci.yml`); enabling required-status-checks is an owner/settings action, not code.
