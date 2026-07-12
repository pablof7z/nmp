# Public surface change log

This is the append-only signoff trail for provisional Rust, UniFFI, Swift, and
Kotlin surface changes. Once merged, existing bytes are immutable; corrections
are new entries. The committed baselines live in [`docs/surface/`](surface/).
The governance-introduction PR is the one-time bootstrap: its base cannot run a
workflow that does not yet exist, so it seeds only the already-merged #67, #73,
and #77 trails below and relies on existing CI plus manual review. Every later
entry is validated against the actual PR context by the trusted base workflow.

## 2026-07-11 — Split indexed filter tags from event tag selection ([PR #67](https://github.com/pablof7z/nmp/pull/67))

- **Failure evidence:** issue #64 showed that one constrained `TagName` type incorrectly represented both NIP-01 indexed filter keys and arbitrary event tag names.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust introduced `IndexedTagName` for `Filter.tags` while selector tag keys became unconstrained strings; FFI, Swift, and Kotlin mirrors changed in the same PR.
- **Persistence impact:** none; this changed query syntax and evaluation, not stored event or journal layout.
- **Diagnostics impact:** wire filters retain one-letter indexed keys; query selection can inspect arbitrary acquired tags without changing diagnostics records.
- **Updated falsifiers:** `nmp-grammar`/`nmp-resolver` contract tests, FFI conversion tests, and Swift/Kotlin filter tests in PR #67.
- **Superseded path removed:** the shared constrained `TagName` type and its whitelist were deleted rather than retained as aliases.
- **Human signoff:** merged through PR #67 on 2026-07-11; the PR review and merge record is the approval trail.

## 2026-07-11 — Rethread FFI over the canonical Rust facade ([PR #73](https://github.com/pablof7z/nmp/pull/73))

- **Failure evidence:** issue #52 proved that direct Rust and FFI callers independently assembled mechanisms and could inherit different acceptance guarantees.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** `nmp-ffi` now delegates to `nmp::Engine`; observer cancellation became opaque and native wrappers were updated for the resulting error/write shapes.
- **Persistence impact:** no store schema change; the facade owns the same memory/redb selection previously duplicated by FFI.
- **Diagnostics impact:** FFI diagnostics are a projection of the canonical facade rather than an independently assembled engine.
- **Updated falsifiers:** facade, FFI conversion/observer, Swift diagnostics, and Kotlin package tests recorded in PR #73.
- **Superseded path removed:** direct `nmp-ffi` mechanism assembly, redundant signed-event verification, and unreachable error variants were deleted.
- **Human signoff:** merged through PR #73 on 2026-07-11; the PR review and merge record is the approval trail.

## 2026-07-11 — Replace aggregate query coverage with scoped acquisition evidence ([PR #77](https://github.com/pablof7z/nmp/pull/77))

- **Failure evidence:** issues #12/#49 showed that aggregate `Coverage` could overstate what the current source plan proved and conflated query evidence with diagnostic intervals.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** query batches now carry `AcquisitionEvidence`; FFI and both native SDKs mirror every source status/auth/shortfall variant while diagnostics retain `CoverageInterval`.
- **Persistence impact:** durable per-filter/per-relay intervals remain the stored fact; the query evidence is derived from the current plan.
- **Diagnostics impact:** diagnostics continue to expose optional exact intervals and compute coalesced wire facts from absorbed atom keys, distinct from query evidence.
- **Updated falsifiers:** engine plan-churn/coalescing/zero-source tests plus exhaustive Rust, Swift, and Kotlin evidence mapping tests in PR #77.
- **Superseded path removed:** `QueryCoverage`, aggregate query `Coverage`, stale facade vocabulary, and the non-optional empty Swift pre-first-batch evidence state were deleted.
- **Human signoff:** merged through PR #77 on 2026-07-11 after local, Atlas, and Nova review; the PR review and merge record is the approval trail.

## 2026-07-11 — Add stable receipt reattachment and truthful restart ambiguity ([PR #83](https://github.com/pablof7z/nmp/pull/83))

- **Failure evidence:** issues #2/#3 showed that a process restart could retain an accepted write in the store while losing the Rust receipt observer, frozen signer work, and per-relay attempt ownership; an at-most-once send crossing that boundary also had no truthful public status distinct from ordinary `GaveUp`.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust exports `ReceiptId`/`ReceiptStream`, adds `Engine::publish_tracked` and `Engine::reattach_receipt`, and adds `WriteStatus::OutcomeUnknown`; UniFFI, Swift, and Kotlin add the corresponding `OutcomeUnknown(relay)` status without collapsing it into `GaveUp`. Native stable-id reattachment APIs remain a later coordinated surface rather than being fabricated in this change.
- **Persistence impact:** Redb now retains versioned, exact-byte `(intent, relay, ordinal)` attempt facts and stable receipt ids; attempt keys are unambiguous, terminal transitions distinguish committed/idempotent-same from missing/conflicting facts, corrupt/unknown versions fail closed, and durable receipt ids are bounded below the disjoint unaccepted-correlation namespace.
- **Diagnostics impact:** no diagnostics schema changes; restart and attempt facts are replayed through receipt streams, while existing diagnostics remain the read-only relay/query projection.
- **Updated falsifiers:** cross-backend attempt ordering/prefix/terminal/corruption/overflow tests, genuine Redb close/reopen recovery tests, exact frozen-signer and boot-before-first-command tests, two-observer future-transition proof, Rust/FFI parity, and exhaustive Swift/Kotlin `OutcomeUnknown` mapping tests.
- **Superseded path removed:** stream-local accepted-write identity, lossy boot reconstruction, blind at-most-once resend/`GaveUp` projection, panic-based attempt decoding, ambiguous relay-prefix keys, and false-success terminal persistence are replaced rather than retained as compatibility paths.
- **Human signoff:** PR #83 is approved under the repository owner's delegated orchestration authority in this session, with independent exact-head review plus the required CI and merge record serving as the approval trail.

## 2026-07-12 — Govern dependency-reexported Rust facade shapes ([PR #90](https://github.com/pablof7z/nmp/pull/90))

- **Failure evidence:** issue #89 showed that the Rust facade snapshot recorded dependency-owned reexports opaquely, so changing a supported enum variant or record field could leave the Rust projection unchanged and produce a false governance verdict.
- **Changed projections:** rust
- **Rust / FFI / Swift / Kotlin impact:** the Rust snapshot now resolves the shapes and root inherent APIs of explicit `nmp` reexports; no UniFFI, Swift, or Kotlin product surface changes in this governance correction.
- **Persistence impact:** none; this changes the generated public-surface evidence and its protected extractor, not stored events, receipts, attempts, or schema versions.
- **Diagnostics impact:** none; diagnostics types are captured more accurately when reexported, but their runtime meaning and values are unchanged.
- **Updated falsifiers:** compiler-backed fixtures cover renamed dependencies, nested structs/enums/aliases, cycles, root methods and associated items, mixed visibility, lock drift, unrelated dependency APIs, numeric-ID rejection, deterministic fresh-clone regeneration, and snapshot size bounds.
- **Superseded path removed:** the opaque dependency-reexport evidence path is eliminated: pinned `cargo-public-api` remains the direct-facade prefix, while a locked, pinned rustdoc-JSON extractor now resolves every explicit dependency-owned reexport and fails closed on unresolved shapes.
- **Human signoff:** PR #90 is approved under the repository owner's delegated orchestration authority in this session, with independent exact-head review, the adversarial governance suite, deterministic regeneration, and repository CI serving as the approval trail.

## 2026-07-12 — Retain write lanes across persistence failures ([PR #91](https://github.com/pablof7z/nmp/pull/91))

- **Failure evidence:** issue #85 showed that `start_attempt` persistence failure could leave a routed relay with no durable Started fact, no wire EVENT, and no owned nonterminal lane; for dynamically resolved `AuthorOutbox`/`ToInboxes` routes, restart with an empty or changed directory could also erase the exact failed relay.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust adds distinct nonterminal `WriteStatus::PersistenceBlocked(relay)` for a durably route-owned lane whose attempt could not start and `WriteStatus::RoutePersistenceBlocked(relay)` for a resolved URL whose route revision itself did not commit; UniFFI, Swift, and Kotlin mirror both without collapsing either into `GaveUp`, `OutcomeUnknown`, or whole-intent `Failed`.
- **Persistence impact:** MemoryStore and RedbStore add typed append-only, canonically ordered resolved-route revisions keyed by `(intent, ordinal)`; the engine commits a complete revision before any corresponding attempt or EVENT, and boot recovers the union of every revision relay and attempt relay without subtracting later directory removals. A failed revision commit persists no URL and therefore makes no false crash-survival claim.
- **Diagnostics impact:** receipt streams gain the two nonterminal persistence-blocked facts; query/relay diagnostics schemas are unchanged, and no blocked lane is reported as terminal or as a wire send.
- **Updated falsifiers:** backend-parity and real-Redb-reopen route-revision tests; a SIGABRT-before-commit rollback/ordinal proof; one-lane, all-lane, mixed-success-plus-ACK, and repeated restart tests; dynamic AuthorOutbox empty-directory recovery; ToInboxes changed-directory/removal recovery with partial revision failure; exhaustive Rust/FFI/parity plus Swift/Kotlin status mappings.
- **Superseded path removed:** the ambiguous empty `pending_relays` sentinel and recovery that depended only on current directory resolution are replaced by separate Started, durable-unstarted, and volatile-route-blocked ownership sets backed by append-only route facts; no compatibility alias or blind retry path remains.
- **Human signoff:** the repository owner's delegated orchestrator approves this surface contract for review in draft PR #91 on 2026-07-12; exact-head adversarial review and required CI remain merge gates.

## 2026-07-12 — Distinguish absent receipts from retained unreadable evidence ([PR #97](https://github.com/pablof7z/nmp/pull/97))

- **Failure evidence:** issue #88 showed that the prior optional/bare-false reattachment result made a never-issued receipt indistinguishable from a retained durable obligation whose receipt, attempt, or route evidence could not be decoded.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust replaces optional reattachment with `ReceiptReattachment::{Attached, NotFound, RetainedButUnreadable}`; UniFFI mirrors the three outcomes, `publish` returns the stable store-issued receipt id, and Swift/Kotlin expose a native typed reattachment result carrying a receipt stream only for `Attached`.
- **Persistence impact:** the canonical store receipt lookup is now fallible; Redb preserves undecodable receipt bytes through boot reconciliation and reports them as unreadable, while corrupt receipt/attempt/route evidence never registers an observer, publishes, or deletes the retained obligation. Terminal retained receipts remain readable and reattachable.
- **Diagnostics impact:** none; receipt reattachment remains a write-observation surface and does not change query or relay diagnostics schemas.
- **Updated falsifiers:** unknown, live, terminal, multiple-observer, genuine-restart, corrupt-receipt, corrupt-attempt, and corrupt-route/lane tests plus exhaustive Rust/FFI/Swift/Kotlin three-variant mapping tests.
- **Superseded path removed:** optional/bare-false reattachment and the infallible Redb receipt decoder are replaced rather than retained as compatibility paths; corruption can no longer collapse into absence or panic during boot reconciliation.
- **Human signoff:** the repository owner's delegated orchestrator approves this surface contract for review in draft PR #97 on 2026-07-12; exact-head adversarial review and required CI remain merge gates.

## 2026-07-12 — Make pre-receipt correlation exhaustion fallible ([PR #100](https://github.com/pablof7z/nmp/pull/100))

- **Failure evidence:** issue #86 showed that the upper-half allocator panicked while issuing its final valid correlation id, so a public publish boundary could crash instead of truthfully reporting that no receipt identity remained.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust adds `EngineError::ReceiptCorrelationIdExhausted`; UniFFI mirrors the fieldless typed error, and Swift/Kotlin map it to native `NMPError` cases. Publish method signatures remain fallible as before, but this capacity boundary is now explicit instead of a panic.
- **Persistence impact:** none; durable store-issued receipt ids remain confined below `2^63`, while the volatile pre-acceptance allocator issues the final upper-half id `2^63` exactly once and then remains exhausted without wrap, reuse, collision, or stored state.
- **Diagnostics impact:** none; no receipt or status stream exists on exhaustion, and query/relay diagnostics are unchanged.
- **Updated falsifiers:** a test-only boundary seed proves the last valid and first/repeated exhausted allocations without `2^63` iterations; runtime, Rust facade, FFI, Swift, and Kotlin mapping tests preserve the typed failure, while existing store boundary tests retain the lower-half proof.
- **Superseded path removed:** the decrement-and-`expect` allocator is replaced by an exhaustion-encoding state and typed publish failure; no sentinel id, compatibility alias, fabricated status, wrap, or reuse path remains.
- **Human signoff:** the repository owner's delegated orchestrator approves this surface contract for review in draft PR #100 on 2026-07-12; exact-head adversarial review and required CI remain merge gates.

## 2026-07-12 — Project unioned relay provenance as reactive row metadata ([PR #109](https://github.com/pablof7z/nmp/pull/109))

- **Failure evidence:** issue #105 showed that `nmp-store` already persists a unioned, per-event-id relay-observation set (`Provenance::seen`), but live-query rows exposed only the bare event -- making strict relay-pinned cache reads (the #1/#63 prerequisite) impossible, and hiding a real state change: the same event later arriving from a second relay produced no update at all, since row equality was event-ID-only.
- **Changed projections:** ffi,kotlin,rust,swift
- **Rust / FFI / Swift / Kotlin impact:** Rust introduces `Row { event, sources: BTreeSet<RelayUrl> }` as the canonical row value and adds `RowDelta::SourcesGrew { id, sources }` (never reusing `Added`, which would falsely claim "newly matches"), carrying the row's full current source set; UniFFI, Swift, and Kotlin mirror both, with `FfiRow` gaining `sources` and `FfiRowDelta` gaining the matching variant.
- **Persistence impact:** none; the underlying relay-observation union was already fully implemented, tested, and durable in `nmp-store` (`EventStore::insert`'s dedup path, survives Redb reopen, correct through signature promotion) -- this PR only projects the already-persisted fact onto the reactive surface that previously discarded it.
- **Diagnostics impact:** none; `AcquisitionEvidence`/relay diagnostics are unchanged and remain orthogonal to per-row provenance (query-plan-level facts vs. per-event-id facts).
- **Updated falsifiers:** a two-relay union proof (union across relays, no-op on identical redelivery, `SourcesGrew` never a second `Added`); the load-bearing lifecycle-trigger proof (an UNRELATED query's own subscribe/unsubscribe recompute must never spuriously emit `SourcesGrew` for a row whose provenance did not change); a genuine Redb close/reopen proving the projection survives restart; exhaustive Rust/FFI/Swift/Kotlin mapping tests proving `SourcesGrew` replaces a row's provenance in place, never duplicating it.
- **Superseded path removed:** `RowDelta::Added`'s bare-`nostr::Event` payload is replaced by `Row` (event + sources) at every call site in the workspace; no compatibility alias or parallel bare-event delivery path remains.
- **Human signoff:** the repository owner's delegated orchestrator approves this surface contract for review in draft PR #109 on 2026-07-12; exact-head adversarial review and required CI remain merge gates.

## 2026-07-12 — Make live-query identity an explicit selection + source + access demand descriptor ([PR #112](https://github.com/pablof7z/nmp/pull/112))

- **Failure evidence:** issue #106 showed that live-query identity and coverage stayed effectively filter-shaped: routing inferred source behavior from filter shape alone, a nested `Derived` demand could inherit its outer's context by accident, and two equal selections under different intended authority had no way to avoid sharing evidence, coverage, or teardown identity merely because their wire filters looked alike (bug-class ledger #18).
- **Changed projections:** rust
- **Rust / FFI / Swift / Kotlin impact:** Rust adds `Demand{selection, source, access, cache}`, `SourceAuthority{AuthorOutboxes, Public}`, `AccessContext{Public}`, `CacheMode{Agnostic, Strict}`, and `ContextualAtom` (the new atom/refcount/coverage identity); `Binding::Derived.inner` is now a full `Demand`, never a bare `Filter`, so an inner query owns its own context and never inherits the outer's. `Demand::from_filter` is the static, total default (any `authors` binding shape -> `AuthorOutboxes`, else `Public`), applied automatically by `LiveQuery::from_filter`. The UniFFI wire schema, Swift, and Kotlin are unchanged in this PR: `FfiDerived.inner` stays `FfiFilter`, and the new Rust-only types apply/strip the same static default transparently at the `nmp-ffi` conversion boundary.
- **Persistence impact:** `CoverageKey`'s identity widens to a full `ContextualAtom` (selection + source + access) and gains a schema-version tag folded into its hash; `RedbStore`'s durable row key gains an independent `"v2:"` version prefix, and `gc()` gains a legacy-row purge pass that deletes any coverage row lacking the current prefix rather than let a pre-#106, filter-only row linger or be silently credited under the new meaning.
- **Diagnostics impact:** none; `AcquisitionEvidence`/`ShortfallFact`'s own public surface is unchanged (`ShortfallFact.atom` stays a bare `ConcreteFilter`, reporting which selection lacks a source, not a context distinction).
- **Updated falsifiers:** a static-default guardrail proving a `$myFollows`-shaped `Derived` authors binding still lowers to `AuthorOutboxes` even though it may resolve empty at runtime; a row-matching-stays-pure guardrail (`root_atoms()` untouched, existing feed/provenance tests green); anti-alias falsifiers at every layer (`ContextualAtom::hash`, `CoverageKey`, router coalescing, `SubId::for_wire`) proving an identical selection under different `SourceAuthority` never aliases evidence, coverage, or wire/attribution identity; a legacy-row-purge falsifier for the versioned coverage-key migration; the full existing `nmp-grammar`/`nmp-resolver`/`nmp-router`/`nmp-store`/`nmp-engine`/`nmp`/`nmp-ffi`/`nmp-bdd`/`nmp-demo`/`nmp-consumer-check`/`nmp-parity` suites stay green (the default `AuthorOutboxes + Public` path reproduces today's exact behavior).
- **Superseded path removed:** bare `ConcreteFilter`/`Filter` as the atom/refcount/coverage identity is replaced by `ContextualAtom`/`Demand` at every call site in the workspace; `SubId::for_filter` and the context-free `coverage_key(&ConcreteFilter)` are removed rather than kept as compatibility aliases; a coverage row computed under the pre-#106 unversioned key format is never reused under the new meaning.
- **Human signoff:** the repository owner's delegated orchestrator approves this surface contract for review in draft PR #112 on 2026-07-12; exact-head adversarial review (atlas's landing review, re-running the hard guardrail proof floors) and required CI remain merge gates.
