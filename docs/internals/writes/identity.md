---
title: "Write identity: Active and Explicit"
category: writes
slug: identity
status: designed
date: 2026-07-29
owns:
  - the `Identity` enum replacing `WriteIntent.identity_override`
  - the ergonomics ruling — a defaulted field, not a constructor argument or a `.with()` chain
  - per-payload identity semantics, and why they differ by payload
  - what #47 established and what survives it verbatim
  - why `Explicit` with no signer parks rather than fails
  - the bech32 boundary rule as it lands on identity, and its concrete FFI casualty
related:
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/payload-and-replaceable-edits.md
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/nip29/group-publication.md
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/internals/conventions/bech32-boundary.md
  - docs/internals/conventions/naming-no-invented-categories.md
issues:
  - "#47 identity override — the semantics this type inherits and restates"
---

# Write identity: Active and Explicit

This document records how a write names the identity it publishes under, as
settled in the 2026-07-28/29 design session with Pablo (repository owner).
Quotes are his, verbatim. It replaces the `Option<PublicKey>` spelling that
#47 built, keeps every guarantee #47 established, and deletes the one thing
#47's spelling forced on apps: knowing your own pubkey before composing.

`status: designed`. §1 is BUILT and describes current master; everything from
§2 on replaces its spelling while preserving its semantics.

---

## 1. What #47 built, and what it costs — BUILT

`WriteIntent.identity_override: Option<PublicKey>` is the current contract,
documented in full on `on_publish` (`crates/nmp/src/core/write.rs:1464-1478`):
`None` means the single-identity contract — the draft must be authored by the
CURRENT active account, else fail closed pre-acceptance. `Some(pk)` is
explicit per-write consent to publish as `pk`: the override must EQUAL the
draft's author ("the reducer never restamps a draft; a mismatch fails closed
with no `Accepted`", `write.rs:1469`), works even while fully logged out, and
acceptance pins `pk` so that "a later `set_active_account` cannot retarget it,
and an override with no registered capability parks durably as
`AwaitingCapability` rather than failing or drifting" (`write.rs:1475-1477`).

The cost is described in `writes/event-builder.md` §1: because the override is
a *comparison* against an author the payload must already carry, every caller
must stamp a pubkey before publishing — including the caller whose whole
request is "just use the active account."

## 2. The ruling — DESIGNED

> about identity: it should default to using the identity active with an optional signer; whether it's on the call to creating the event builder or a .with().. I don't have a strong preference one way or the other; whatever is more ergonomic.

The type:

```rust
pub enum Identity {
    Active,                 // whoever is active at acceptance time
    Explicit(PublicKey),    // this key's signer, active or not
}
```

replacing `identity_override: Option<PublicKey>` on `WriteIntent` outright —
no wrapper, no alias, per `conventions/no-backwards-compatibility.md`. The
enum is not `Option` renamed: `Active` is a positive statement ("resolve the
active account at acceptance and pin it"), not the absence of one, and the
name shows up in receipts, diagnostics, and app code where `None` would say
nothing.

## 3. Ergonomics: a defaulted field — DESIGNED

Pablo left the spelling open ("whatever is more ergonomic"), naming two
candidates: a constructor argument, or a `.with()` chain. The answer is
**neither** — `identity` is a *defaulted field* on `WriteIntent`, defaulting
to `Active`.

Why not `.with()`: UniFFI Records have no methods, so a `.with_identity()`
chain cannot project across FFI at all — the pattern would exist in Rust and
be unspeakable in Swift/Kotlin, splitting the API's shape per platform for no
gain. Why not a required constructor argument: it taxes the common case. The
overwhelming majority of writes publish as the active account; forcing every
one of them to say so is exactly the ceremony this design exists to delete.

A defaulted field is the one spelling that is idiomatic on *both* sides of
the boundary: in Rust, `WriteIntent` stays the all-pub record that composers
and tests already construct (`..Default::default()` or struct literal); at
FFI, `#[uniffi(default = ...)]` makes it an omittable labeled argument — the
exact pattern the current `identity_override` already uses
(`crates/nmp-ffi/src/types.rs:626`). The common case writes nothing; the
explicit case writes `identity: Identity::Explicit(pk)`.

## 4. Per-payload semantics — DESIGNED, and the differences are the point

`Identity` means something different per payload variant, and the difference
is not an irregularity to smooth over — it is a precise statement of where an
author can come from.

**For a builder payload (`WritePayload::Event`), `Identity` SELECTS the
author.** An `EventBuilder` structurally cannot carry a pubkey
(`writes/event-builder.md` §3), so there is nothing to compare the identity
against and the author/override mismatch class is unrepresentable. `Active`
resolves the active account at acceptance and stamps it; `Explicit(pk)`
stamps `pk`. Selection, not verification — the identity is the *only* source
of the author.

**For `Signed(Event)`, `Identity` RESTATES the author, and the existing
consent-restatement check survives verbatim.** A signed event's author is
intrinsically frozen in its bytes; no identity choice can change it. So
`Explicit(pk)` must equal `event.pubkey`. This check exists on master today,
with the same shape and the same reasoning, at
`crates/nmp/src/core/write.rs:1626-1641`:

> Already-signed payloads are verified verbatim and never ask a local signer,
> so their author is intrinsically frozen. An explicit override may still name
> that author (a harmless restatement) — but naming anyone ELSE is a
> consent/author contradiction and fails closed before acceptance (#47).

That is the one payload that still *states* an author, so it is the one place
a comparison still has two operands — and the one place #47's fail-closed
check remains as a check rather than becoming structure. Note what `Signed` +
`Active` means under this design: the event's own author, whoever it is. A
signed event needs no signer, so `Active` imposes no active-account
requirement on it — this is what makes "republish someone else's signed event
to my archive relay" (Pablo's own case, see `routing/auto-and-explicit.md`)
publishable while logged out, with no identity involved at all.

The asymmetry generalizes: **wherever an author is absent, identity selects;
wherever an author is stated, identity may only restate.** Any future payload
variant must land on one side of that line, deliberately.

## 5. What #47 established, and what survives — DESIGNED (semantics BUILT)

Every #47 guarantee survives this redesign unchanged; only the spelling of
the input moves. Verified against master:

- **Active-identity resolution is pinned at acceptance.** Acceptance pins the
  resolved key (`expected_pubkey` / `signing_identity_ref`,
  `write.rs:1472-1478`), so everything downstream — the frozen body,
  `RequestSign`, the `SignerAttached` re-arm, restart replay — targets it
  forever. A later `set_active_account` cannot retarget an accepted write.
  Under the builder this pin matters MORE, not less: with `Active`, the
  moment of acceptance is what converts "whoever is active" into one concrete
  pubkey stamped into the frozen body. Without the pin, a queued write's
  author would float with the session — the account-switch retargeting bug
  #47 exists to prevent, reintroduced through the convenience path.
- **`Active` with no active account fails closed pre-acceptance.** Master:
  "unsigned publish requires an active account" (`write.rs:1617-1627`) — no
  `Accepted`, no journal row, no effect. Unchanged: `Active` is a resolution
  *instruction*, and an instruction that cannot resolve at acceptance is a
  refusal, not a parked hope. Nothing is pinned, so nothing may park.
- **`Explicit(pk)` with no registered signer for `pk` PARKS durably as
  `AwaitingCapability`.** Master behavior (`write.rs:1806-1850`: the intent
  is retained and the status emitted "so the exact frozen identity can be
  reattached"), and it stays correct under the redesign — parking is that
  variant's *whole purpose*, not an accident of it. `Explicit(pk)` is a
  complete, self-sufficient statement of intent: the author is known (so the
  body can be frozen and journaled), only the capability is missing. Failing
  instead would delete the two workflows the variant exists for:
  publish-while-logged-out, and late signer attachment (a NIP-46 remote
  signer that connects minutes after the app queued the write). The park is
  not a silent limbo: it is visible on the receipt stream as
  `AwaitingCapability`, replayed on reattach, and cancellable — a parked
  write is a decision the app can always observe and revoke. Contrast with
  `Active`'s fail-closed above: `Explicit` names its key, so there is
  something concrete to wait for; `Active` without an account names nothing,
  so there is nothing to park.

## 6. Bech32 stops at the app's boundary — DESIGNED

Pablo's rule, in full (answering "Identity at FFI: `Option<String>` vs a real
type"):

> B. But .explicit shouldn't be an npub... npub (any bech32) is outward-facing decoration; internally it should be a pubkey. Nothing internal should use bech32 encoding, bech32 encodings are to show something to the user or to receive something from a user (e.g. "this is user npub1...." (displaying something to the external user), or the app saying nmp "load nevent1...." (because the user copy-pasted an nevent1))

So `Identity::Explicit(PublicKey)` — a real key type, never a string, never
bech32. The full statement of the rule and its other consequences live in
`conventions/bech32-boundary.md`; what this document owns is the concrete
casualty on the identity surface.

**The casualty:** `FfiWriteIntent.identity_override` today accepts hex OR
bech32 npub *by design*. Its own doc comment
(`crates/nmp-ffi/src/types.rs:609-613`) states the identity is passed "as
64-char hex (the module-wide `convert::parse_pubkey` rule every other pubkey
input here follows) or bech32 `npub` (an identity is the one input an app
most plausibly holds in display form)". That parenthetical is a genuine
argument — apps really do hold identities in display form — and under this
rule it is genuinely wrong: holding a value in display form is a fact about
the app's UI, and the place display forms are decoded is therefore the app's
own boundary, not NMP's write plane. The app decodes with
`decode_nostr_entity` (the exported bech32 codec,
`crates/nmp-ffi/src/entity.rs`) at the moment it *receives* the display form
from a user, and hands NMP a pubkey.

The failure mode the old leniency invites is boundary creep: once one write
field accepts either encoding "for convenience", every pubkey-shaped input is
an argument about which encodings it takes, error surfaces double (invalid
hex vs invalid bech32 vs valid-bech32-wrong-variant — the `nsec` refusal in
`crates/nmp-ffi/src/convert.rs:177` exists because of exactly this), and
"what does NMP accept here" stops having one answer. One decode door
(`decode_nostr_entity`), one internal representation (`PublicKey`), zero
bech32 below the app boundary.
