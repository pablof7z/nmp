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
