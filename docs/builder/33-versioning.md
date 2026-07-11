# Governed provisional public surface

**Status: CURRENT POLICY.** NMP has not promised v2 compatibility, but public
shape changes are governed. "Provisional" means change is possible with
evidence and signoff, not that any surface may drift casually.

## The posture

The two-noun thesis and promoted behavioral invariants direct the work. Public
names, enum cases, records, FFI layouts, and platform spellings remain
provisional until they survive cross-platform falsification and earn a v2
compatibility contract.

This leaves room to correct a shape that cannot express source-scoped evidence,
durable signer waiting, contextual protocol authority, or explicit shortfall.
It does not license an unreviewed breaking change.

## What may change before v2

- `NMPFilter` may become a richer demand descriptor whose identity includes
  selection, source authority, and access context.
- Aggregate `Coverage` may be replaced by cache/acquisition/shortfall evidence.
- `WriteIntent`, receipt status, pending-row state, and signer-provider records
  may change to carry the durable contract.
- `setActiveAccount` may split into a current-pubkey input plus independent
  signer registration/default/override behavior.
- Diagnostics will grow raw source, connection, AUTH, retry, error, and limit
  facts.
- Protocol module and Kotlin projections remain target surfaces and will move
  as their falsifiers expose pressure.

These are examples of known pressure, not a promise of exact names.

## Required change discipline

Every proposed public-shape change must:

1. state what the current shape cannot express or makes unsafe;
2. map impact across the canonical Rust facade, persistence, diagnostics, FFI,
   Swift, Kotlin, and enabled protocol modules;
3. update affected projections and falsifiers together;
4. receive explicit human architecture signoff; and
5. remove the superseded path instead of preserving premature parallel APIs.

A passing local implementation test is not enough. The supported facade and at
least the platform falsifier affected by the change must agree on behavior.

## What remains the frame

Before v2, even behavioral decisions can be revisited when new evidence exposes
a flaw. They are nevertheless the current architecture frame and cannot be
eroded accidentally:

- NMP is an embeddable engine, not an app framework.
- Live query and write intent remain the two workload nouns.
- Engine decisions consume closed, introspectable values; app closures stay
  after delivery.
- Core has no preferred content kind or product feed policy.
- Protocol modules own exact schemas and compose immutable unsigned drafts.
- Query evidence is source-scoped and never global Nostr truth.
- Durable `Accepted` means persisted obligation plus canonical pending row.
- Signer choice is defaultable, overridable, and pinned at acceptance.
- One engine is one shared local trust domain with explicit destructive reset.
- Limits and overload produce explicit shortfall/backpressure, never silent
  first-N substitution.

Changing one of these requires an explicit architecture decision and ledger /
design-document update, not an incidental API patch.

## Building on NMP before v2

1. Pin a commit or tag and upgrade deliberately.
2. Distinguish `CURRENT`, `PARTIAL`, and `TARGET` documentation.
3. Watch the bug-class ledger and known gaps for behavioral movement.
4. Keep product-specific query composition in the app. Use opt-in protocol
   modules only for their exact protocol-semantic surface as it becomes built.
5. Expect synchronized migrations when a governed public change lands; do not
   assume today's name is frozen merely because its current test passes.

## What v2 earns

v2 turns the surviving cross-platform surface into a compatibility contract
with ordinary versioning and deprecation discipline. The project freezes an
earned contract after falsification, not a guess before it. Until then the
accurate promise is: **provisional shapes, protected review discipline, and
truth-anchored current/target documentation.**

---

<!-- nav-footer -->
<sub>← [Extending NMP](32-extending.md) · [Index](README.md)</sub>
