---
title: "Write identity: Active and Explicit"
category: writes
slug: identity
status: built
date: 2026-07-29
owns:
  - **who may choose the `Identity` value — one mechanism, never a capability
    crate (owner ruling, 2026-08-17), and the three crates violating it today**
  - the `Identity` enum replacing `WriteIntent.identity_override`
  - the ergonomics ruling — a defaulted field, not a constructor argument or a `.with()` chain
  - per-payload identity semantics, and why they differ by payload
  - what #47 established and what survives it verbatim
  - why `Explicit` with no signer parks rather than fails
  - the bech32 boundary rule as it lands on identity, and its concrete FFI casualty
related:
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/durable-replaceable-operations.md
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/nip29/group-publication.md
issues:
  - "#47 identity override — the semantics this type inherits and restates"
---

# Write identity: Active and Explicit

This document records how a write names the identity it publishes under, as
settled in the 2026-07-28/29 design session with Pablo (repository owner).
Quotes are his, verbatim. It replaces the `Option<PublicKey>` spelling that
#47 built, keeps every guarantee #47 established, and deletes the one thing
#47's spelling forced on apps: knowing your own pubkey before composing.

All of it is built. `Identity` shipped in #1009 (commit `1f11ebc0`), replacing
`identity_override` outright; §1 records what #47's spelling was and why it
died.

---

## 1. What #47 built, and what it cost

`WriteIntent.identity_override: Option<PublicKey>` was the contract #47 built.
`None` meant the single-identity contract — the draft had to be authored by the
current account, else fail closed pre-acceptance. `Some(pk)` was explicit
per-write consent to publish as `pk`: the override had to EQUAL the draft's
author (the reducer never restamps a draft; a mismatch failed closed with no
`Accepted`), worked while fully logged out, and acceptance pinned `pk` so a
later current-account change could not retarget it. An override with no
registered capability parked durably as `AwaitingCapability` rather than
failing or drifting.

`identity_override` no longer exists anywhere in `crates/` — #1009
(`1f11ebc0`) deleted it along with its FFI mirror. The semantics above all
survive on `Identity`; the spelling did not.

The cost that killed it is described in `writes/event-builder.md` §1: because
the override was a *comparison* against an author the payload had to already
carry, every caller had to stamp a pubkey before publishing — including the
caller whose whole request was "just use the current account."

## 2. The ruling

> about identity: it should default to using the identity active with an optional signer; whether it's on the call to creating the event builder or a .with().. I don't have a strong preference one way or the other; whatever is more ergonomic.

The type:

```rust
pub enum Identity {
    Active,                 // the current account at acceptance time
    Explicit(PublicKey),    // this public key, whether current or not
}
```

ships at `crates/nmp-grammar/src/write.rs:311-326`, having replaced
`identity_override: Option<PublicKey>` on `WriteIntent` outright — no wrapper,
no alias. The
enum is not `Option` renamed: `Active` is a positive statement ("resolve the
current account at acceptance and pin it"), not the absence of one, and the
name shows up in receipts, diagnostics, and app code where `None` would say
nothing.

## 3. Ergonomics: a defaulted field

Pablo left the spelling open ("whatever is more ergonomic"), naming two
candidates: a constructor argument, or a `.with()` chain. The answer was
**neither** — `identity` is a *defaulted field* on `WriteIntent`, defaulting
to `Active` (`#[default] Active`, `crates/nmp-grammar/src/write.rs:315`).

Why not `.with()`: UniFFI Records have no methods, so a `.with_identity()`
chain cannot project across FFI at all — the pattern would exist in Rust and
be unspeakable in Swift/Kotlin, splitting the API's shape per platform for no
gain. Why not a required constructor argument: it taxes the common case. The
overwhelming majority of writes publish as the current account; forcing every
one of them to say so is exactly the ceremony this design exists to delete.

A defaulted field is the one spelling that is idiomatic on *both* sides of
the boundary: in Rust, `WriteIntent` stays the all-pub record that composers
and tests already construct (`..Default::default()` or struct literal); at
FFI, `#[uniffi(default = ...)]` would make it an omittable labeled argument.

That last half did not survive contact with UniFFI. `FfiWriteIntent.identity`
carries no default and must be stated
(`crates/nmp-ffi/src/types.rs:849-857`): UniFFI 0.29 record defaults accept
only literals, so an enum-valued default is not expressible at that boundary
at all. The ergonomic native tiers — `NMP`'s `WriteIntent`,
`com.nmp.sdk.WriteIntent` — default it to `.active` in their own language,
which is where app code writes it. In Rust the common case still writes
nothing; the explicit case writes `identity: Identity::Explicit(pk)`.

## 4. Per-payload semantics, and why the differences are the point

`Identity` means something different per payload variant, and the difference
is not an irregularity to smooth over — it is a precise statement of where an
author can come from.

**For a builder payload (`WritePayload::Event`), `Identity` SELECTS the
author.** An `EventBuilder` structurally cannot carry a pubkey
(`writes/event-builder.md` §3), so there is nothing to compare the identity
against and the author/override mismatch class is unrepresentable. `Active`
resolves the current account at acceptance and stamps it; `Explicit(pk)`
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
signed event needs no signer, so `Active` imposes no current-account
requirement on it — this is what makes "republish someone else's signed event
to my archive relay" (Pablo's own case, see `routing/auto-and-explicit.md`)
publishable while logged out, with no identity involved at all.

The asymmetry generalizes: **wherever an author is absent, identity selects;
wherever an author is stated, identity may only restate.** Any future payload
variant must land on one side of that line, deliberately.

## 5. What #47 established, and what survives

Every #47 guarantee survived the redesign unchanged; only the spelling of the
input moved. Verified against the current tree:

- **`Identity::Active` resolution is pinned at acceptance.** Acceptance pins the
  resolved key (`expected_pubkey` / `signing_identity_ref`,
  `crates/nmp-engine/src/core/write.rs:2943-2949`), so everything downstream — the frozen body,
  `RequestSign`, the `SignerAttached` re-arm, restart replay — targets it
  forever. A later current-account change cannot retarget an accepted write.
  Under the builder this pin matters MORE, not less: with `Active`, the
  moment of acceptance is what converts "whoever is active" into one concrete
  pubkey stamped into the frozen body. Without the pin, a queued write's
  author would float with the session — the account-switch retargeting bug
  #47 exists to prevent, reintroduced through the convenience path.
- **`Active` with no current account fails closed pre-acceptance.**
  `PublishError::NoCurrentAccount`
  (`crates/nmp-engine/src/core/write.rs:3020`) — no
  `Accepted`, no journal row, no effect. Unchanged: `Active` is a resolution
  *instruction*, and an instruction that cannot resolve at acceptance is a
  refusal, not a parked hope. Nothing is pinned, so nothing may park.
- **`Explicit(pk)` with no available signing provider for `pk` PARKS durably as
  `AwaitingCapability`.** The intent is retained and the status emitted so the
  exact frozen identity can resume when its provider becomes available
  (`AwaitingCapability`, `crates/nmp-engine/src/core/write.rs`; the FFI mirror
  spells the same park `FfiSigningState::AwaitingSigner`). Parking is that
  variant's *whole purpose*, not an accident of it. `Explicit(pk)` is a
  complete, self-sufficient statement of intent: the author is known (so the
  body can be frozen and journaled), only the capability is missing. Failing
  instead would delete the two workflows the variant exists for:
  publish-while-logged-out, and a configured provider that becomes available
  minutes after the app queued the write. The park is
  not a silent limbo: it is visible on the receipt stream as
  `AwaitingCapability`, replayed when capability becomes available, and cancellable — a parked
  write is a decision the app can always observe and revoke. Contrast with
  `Active`'s fail-closed above: `Explicit` names its key, so there is
  something concrete to wait for; `Active` without an account names nothing,
  so there is nothing to park.

## 6. Bech32 stops at the app's boundary

Pablo's rule, in full (answering "Identity at FFI: `Option<String>` vs a real
type"):

> B. But .explicit shouldn't be an npub... npub (any bech32) is outward-facing decoration; internally it should be a pubkey. Nothing internal should use bech32 encoding, bech32 encodings are to show something to the user or to receive something from a user (e.g. "this is user npub1...." (displaying something to the external user), or the app saying nmp "load nevent1...." (because the user copy-pasted an nevent1))

So `Identity::Explicit(PublicKey)` — a real key type, never a string, never
bech32. The full statement of the rule and its other consequences live in
; what this document owns is the concrete
casualty on the identity surface.

**The casualty:** `FfiWriteIntent.identity_override` used to accept hex OR
bech32 npub *by design*, on the argument that "an identity is the one input an
app most plausibly holds in display form". That is a genuine argument — apps
really do hold identities in display form — and under this rule it is
genuinely wrong: holding a value in display form is a fact about the app's UI,
and the place display forms are decoded is therefore the app's own boundary,
not NMP's write plane. `FfiIdentity::Explicit { pubkey: String }`
(`crates/nmp-ffi/src/types.rs:820-841`) now takes 64-char hex and nothing
else; a well-formed `npub` is refused with
`FfiError::InvalidPublicKey` before any engine call. The app decodes with
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

## 7. A capability crate never resolves the author — OWNER RULING, 2026-08-17

`Identity` says which account a write publishes under. This section says who
is allowed to *choose that value*, which every earlier section left open, and
the answer is: one mechanism, never a capability crate.

Owner ruling, verbatim:

> capabilities (if by that you mean nip29, nip02) shouldn't be determining the
> author themselves, that's not DRY, nor SRP, it should be the underlying
> mechanism (event builder I guess?) that does that; not redoing the same
> fucking code in a million places. like on swift it could be
> `nmp.follow(bob, as: alice)` or `nmp.follow(bob)` (follow with the current
> account)

Two rules follow.

**A capability crate does not read the session.** Resolving "who is the
current account" is universal — every kind, every NIP — so it belongs to the
write plane, which already owns it: `Identity::Active` is a resolution
instruction the engine executes and pins at acceptance (§5). A capability that
reads `current_pubkey` itself and hands back `Identity::Explicit` has
re-implemented that resolution one layer above the boundary that owns it, and
has done so in every capability crate separately.

**The app-facing shape is an optional account, defaulted.** `nmp.follow(bob)`
follows as the current account; `nmp.follow(bob, as: alice)` names one. The
capability names the operation; it never names an account unless the caller
did.

This is the same rule §1 of `writes/event-builder.md` already states for the
old author-bearing draft — *"The failure is universal — every kind, every NIP
— which is why it must not be solved inside any protocol crate"* — applied to
the form it came back in. #838 removed NIP-29's `groupMessageIntent` for
deriving author and time internally; reading the session inside a capability
is that defect wearing different clothes.

### The standing violation

Three capability crates do exactly what this ruling forbids today, each with
its own copy of the same four lines — read `engine.session()`, take
`current_pubkey`, refuse when signed out, stamp `Identity::Explicit(author)`:

- `crates/nmp-nip02/src/observe.rs` — `set_following`
- `crates/nmp-bookmarks/src/writes.rs` — `publish_operation`, behind the
  bookmark add/remove doors
- `crates/nmp-nip29/src/group_list_writes.rs` — the group-list add/remove
  doors

`nmp-nip29`'s `Group` doors take the opposite approach and require the caller
to pass `author: PublicKey` on every operation, which is the same rule broken
from the other side: the account becomes mandatory where the ruling makes it
optional.

The correct shape is neither: the capability passes the caller's choice
through — an optional account, absent by default — and the write plane
resolves it. This is recorded as a defect, not a precedent.
