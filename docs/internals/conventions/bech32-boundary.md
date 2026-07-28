---
title: Bech32 lives only at the human boundary
category: conventions
slug: bech32-boundary
status: policy
date: 2026-07-29
owns:
  - where bech32 (npub, nevent, naddr, …) is allowed to exist
  - the rule that everything internal uses the decoded type
  - the sanctioned boundary decoder
  - the known standing violation and what happens to it
related:
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/internals/conventions/naming-no-invented-categories.md
  - docs/internals/writes/identity.md
issues: []
---

# Bech32 lives only at the human boundary

Pablo (repository owner, 2026-07-28), answering "Identity at FFI:
`Option<String>` vs a real type":

> B. But .explicit shouldn't be an npub... npub (any bech32) is
> outward-facing decoration; internally it should be a pubkey. Nothing
> internal should use bech32 encoding, bech32 encodings are to show something
> to the user or to receive something from a user (e.g. "this is user
> npub1...." (displaying something to the external user), or the app saying
> nmp "load nevent1...." (because the user copy-pasted an nevent1))

---

## 1. The rule — POLICY

Bech32 encodings (`npub`, `nevent`, `naddr`, and the rest) exist for exactly
two things:

1. **Showing something to a human** — his example: "this is user npub1...."
   (displaying something to the external user).
2. **Receiving something a human pasted** — his example: the app saying nmp
   "load nevent1...." (because the user copy-pasted an nevent1).

Everything internal — every struct field, function parameter, FFI record,
journal entry, identity value — uses the decoded type: `PublicKey`, `EventId`,
coordinates. A bech32 string appearing anywhere that is not directly a display
surface or a paste-acceptance surface is a defect. The bech32 form is
"outward-facing decoration", not an identity representation; it decodes at the
boundary and the decoded value is what travels.

## 2. The sanctioned boundary decoder — BUILT

`decode_nostr_entity` is exported from the facade
(`crates/nmp/src/lib.rs:220`, `pub use nmp_grammar::{decode_nostr_entity,
NostrEntity, NostrEntityError};`). That is where a pasted `nevent1...` /
`npub1...` / `naddr1...` becomes a typed `NostrEntity` — at the app's own
boundary, once, on the way in. Past that call, bech32 does not exist.

## 3. The known standing violation — BUILT, and wrong under this rule

`FfiWriteIntent.identity_override` (`crates/nmp-ffi/src/types.rs:626-627`)
accepts hex **or** bech32 npub *by design*. Its doc comment
(`types.rs:609-613`) says so explicitly:

> `nmp::WriteIntent::identity_override` mirror (#47 Unit A): the
> identity this ONE write is published under, as 64-char hex (the
> module-wide `convert::parse_pubkey` rule every other pubkey input
> here follows) or bech32 `npub` (an identity is the one input an app
> most plausibly holds in display form).

The parenthetical is the exact reasoning this rule rejects: "the app most
plausibly holds it in display form" is an argument for the **app** decoding at
its own human boundary, not for NMP's internal surface accepting display
encoding. Under this rule the app decodes (via `decode_nostr_entity` or its
platform equivalent) and passes a pubkey; the npub acceptance in
`identity_override` is a defect to be deleted — deleted, not deprecated
(`docs/internals/conventions/no-backwards-compatibility.md`) — when the
identity surface is reworked (`docs/internals/writes/identity.md`, where
ruling 7 of the write-plane session made `Identity::Explicit(PublicKey)`,
never bech32, the settled shape).

## 4. How to apply it when reviewing — POLICY

- A `String` parameter documented as "hex or npub" is the smell; the fix is a
  typed key and one decode call at the true human boundary.
- "The app probably has the npub handy" is never a justification — that
  convenience belongs in the app's input-handling layer, where the human is.
- Display formatting (`npub` for showing) likewise belongs to the rendering
  edge, not to any value NMP stores or transports.
