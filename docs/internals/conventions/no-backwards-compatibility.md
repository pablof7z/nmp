---
title: No backwards compatibility — the old spelling dies in the same change
category: conventions
slug: no-backwards-compatibility
status: policy
date: 2026-07-29
owns:
  - the rule that a replaced public API is deleted, never aliased or deprecated
  - how proposals must be presented under this rule
  - why breaking callers is cheap here and a second API is not
related:
  - docs/internals/conventions/bech32-boundary.md
  - docs/internals/conventions/naming-no-invented-categories.md
  - docs/internals/conventions/schema-epoch-discard.md
  - docs/internals/writes/event-builder.md
  - docs/internals/nip29/group-publication.md
issues: []
---

# No backwards compatibility — the old spelling dies in the same change

Pablo (repository owner, 2026-07-28):

> no backwards compatibility!!!! I told you this so many times!!!

This is standing policy, restated here so it never has to be restated to him
again.

---

## 1. The rule — POLICY

When a better spelling of something is adopted, the old spelling is **DELETED
in the same change**. Never:

- an alias kept alongside the new name;
- a deprecation period, marker, or warning;
- a wrapper that forwards the old API to the new one;
- any second way to express the same thing, however temporary it is claimed
  to be.

The workspace, the FFI layer, Swift, Kotlin, and every test move to the new
spelling in the same change. "Temporary" compatibility paths are not
temporary; they are a second API with a maintenance bill and no deletion date.

The same rule applies to durable bytes. A `SCHEMA_VERSION` bump makes every
non-current store unsupported; the consumer discards and recreates it instead
of NMP carrying a decoder, migration, adoption, alias, or outbox-drain path.
That action loses more than a read cache. The complete policy and operator cost
live in [`schema-epoch-discard.md`](schema-epoch-discard.md).

## 2. Consequences for how proposals are presented — POLICY

This rule binds *proposal-writing*, not just implementation, because the
places it gets violated are analyses and option lists:

- **Do not offer "replace vs wrap" as a choice.** Wrapping is never an option,
  so a proposal that presents it as one is presenting a non-option. During the
  2026-07-28 write-plane session, the question "does `Identity` replace
  `identity_override` outright (breaking) or wrap it?" was retracted as
  invalid the moment this ruling landed — wrapping was never on the table.
- **Do not weigh breaking Swift/Kotlin snapshots as an argument
  against a better design.** Migration cost of in-workspace callers is not a
  design input. If the design is better, the callers move.
- **Do not propose migration shims, phased rollouts of a spelling, or
  "keep both until X".** There is no X.

What remains legitimate: arguing that the new design is not actually better.
The rule removes compatibility as a counterweight; it does not remove design
review.

## 3. The reasoning — POLICY

### 3.1 There are no external consumers

The entire cost that backwards compatibility exists to avoid is **absent
here**. NMP has no external consumers. Every caller of every public API — the
facade, the FFI component, the Swift and Kotlin SDKs, the protocol crates, the
harnesses, the example apps — is inside this workspace or in a sibling
repository that moves with it. Nobody is pinned to a version we cannot reach
and update in the same change.

Compatibility policy is a tax paid to strangers. There are no strangers. Paying
it anyway buys nothing and costs a permanent second API.

This is a *fact about the current situation*, not a value judgement, and it is
the load-bearing premise of the whole rule: an argument for compatibility must
first establish a consumer who would be broken. If it cannot name one, it is
not an argument.

### 3.2 Clean architecture is the absolute priority

Where the two conflict, **architectural cleanliness wins outright** — not on
balance, not usually, but absolutely. A design that is correct and singular
beats a design that is compatible. The old spelling has no standing to
influence the new one.

The practical form: when replacing something, do not let its shape leak into
the replacement to ease migration. Do not keep a field because callers set it,
do not keep a parameter because a composer passes it, do not keep a variant
because a match arm exists. Design the thing as if the old one had never
existed, then move every caller.

### 3.3 Two spellings is itself a defect

Independent of migration entirely: every reader must learn both, every tool
must handle both, and every future change must be made twice or drift. That
cost is permanent and compounds. The cost it is traded against — moving
in-workspace callers once — is small, bounded, and paid by the same change that
creates the value.

This is the same instinct behind #838's deletion of `publish_composed` (a
second write lifecycle) and behind refusing `group.observe` as a second read
door (`docs/internals/nip29/group-publication.md` §3): one mechanism, one
spelling, one door.

## 4. Precedent in the tree — BUILT

The grammar's write module opens with this rule already applied, verbatim
(`crates/nmp-grammar/src/write.rs:12-13`):

> Hard break, no compatibility alias: every caller in the workspace moved
> to `nmp_grammar::{Durability, WriteIntent, ...}` in the same change.

That is the template every replacement follows: the commit that introduces the
new spelling is the commit in which the old one no longer exists.
