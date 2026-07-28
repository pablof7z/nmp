# CLAUDE.md

`AGENTS.md` is the canonical contributor guide for this repository and applies
to agents and humans alike. **Read it first.** This file exists only so the
rules are not missed by a tool that looks for this filename, and it deliberately
duplicates nothing beyond the three standing conventions below.

## The three that get violated most

These are violated in *proposals* far more often than in code, which is why they
are repeated here. Full reasoning and the incidents behind each are in
`docs/internals/conventions/`.

### 1. No backwards compatibility, ever

`docs/internals/conventions/no-backwards-compatibility.md`

A replaced spelling is **DELETED in the same change**. No alias, no deprecation
period, no forwarding wrapper, no "keep both until X" — there is no X.

Two facts make this absolute rather than aspirational:

- **NMP has no external consumers.** Every caller of every surface is inside
  this workspace or a sibling repository that moves with it. Compatibility
  policy is a tax paid to strangers, and there are no strangers. An argument
  for compatibility must first name a consumer who would break; if it cannot,
  it is not an argument.
- **Clean architecture is the absolute priority.** Where cleanliness and
  compatibility conflict, cleanliness wins outright — not on balance. Design
  the replacement as if the old thing had never existed, then move every
  caller. Do not let the old shape leak into the new one to ease migration.

Consequences when writing a proposal: never present "replace vs wrap" as a
choice, and never weigh breaking Swift/Kotlin/governed snapshots as an argument
against a better design. Arguing the new design is not actually better remains
entirely legitimate.

### 2. Bech32 only at the user boundary

`docs/internals/conventions/bech32-boundary.md`

`npub`, `nevent`, `naddr` are for showing something to a human, or accepting
what a human pasted. Nothing internal uses them — parameters, struct fields,
FFI arguments and protocol-crate signatures all take the decoded type
(`PublicKey`, `EventId`). An app decodes at its own boundary and hands NMP a
key.

### 3. No invented categories, no repo jargon

`docs/internals/conventions/naming-no-invented-categories.md`

Do not name a category the protocol does not have, and do not let internal
shorthand harden into vocabulary. "Foreign kinds" described a category that
does not exist in Nostr — NIP-29 owns no event schemas, so there was nothing
for a schema to be foreign *to*. It spread to 13 sites and became load-bearing
in a CI gate before being deleted (#960). A wrong term gets more expensive the
longer it lives.

## Everything else

`AGENTS.md` — cold-start reading order, the issue-first rule, the six
architecture review gates, and working discipline (isolated worktrees, one PR
per coherent unit, never push to `master` from a shared build).
