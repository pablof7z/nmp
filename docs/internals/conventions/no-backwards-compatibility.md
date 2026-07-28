---
title: No backwards compatibility — the old spelling dies in the same change
category: conventions
slug: no-backwards-compatibility
status: policy
date: 2026-07-29
owns:
  - the rule that a replaced surface is deleted, never aliased or deprecated
  - how proposals must be presented under this rule
  - why breaking callers is cheap here and a second surface is not
related:
  - docs/internals/conventions/bech32-boundary.md
  - docs/internals/conventions/naming-no-invented-categories.md
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
- a wrapper that forwards the old surface to the new one;
- any second way to express the same thing, however temporary it is claimed
  to be.

The workspace, the FFI layer, Swift, Kotlin, the governed surface snapshots,
and every test move to the new spelling in the same change. "Temporary"
compatibility surfaces are not temporary; they are a second surface with a
maintenance bill and no deletion date.

## 2. Consequences for how proposals are presented — POLICY

This rule binds *proposal-writing*, not just implementation, because the
places it gets violated are analyses and option lists:

- **Do not offer "replace vs wrap" as a choice.** Wrapping is never an option,
  so a proposal that presents it as one is presenting a non-option. During the
  2026-07-28 write-plane session, the question "does `Identity` replace
  `identity_override` outright (breaking) or wrap it?" was retracted as
  invalid the moment this ruling landed — wrapping was never on the table.
- **Do not weigh breaking Swift/Kotlin/governed snapshots as an argument
  against a better design.** Migration cost of in-workspace callers is not a
  design input. If the design is better, the callers move.
- **Do not propose migration shims, phased rollouts of a spelling, or
  "keep both until X".** There is no X.

What remains legitimate: arguing that the new design is not actually better.
The rule removes compatibility as a counterweight; it does not remove design
review.

## 3. The reasoning — POLICY

Two ways to say one thing is **itself a defect**, independent of any migration
concern: every reader must learn both, every tool must handle both, and every
future change must be made twice or drift. Meanwhile the cost the alias is
supposed to avoid is small here by construction — the workspace moves
together, all callers are in reach of one change, so breaking them is cheap. A
permanent second surface is not cheap; it is permanent.

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
