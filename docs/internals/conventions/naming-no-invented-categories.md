---
title: Naming — no invented categories, no repo jargon
category: conventions
slug: naming-no-invented-categories
status: policy
date: 2026-07-29
owns:
  - the rule against inventing categories the protocol does not have
  - the rule against internal shorthand hardening into vocabulary
  - the "foreign kind" defect as the worked example, and its fix (PR #960)
  - why terminology defects get more expensive the longer they live
related:
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/internals/conventions/bech32-boundary.md
  - docs/internals/nip29/group-publication.md
issues: []
---

# Naming — no invented categories, no repo jargon

Pablo (repository owner, 2026-07-28), on finding "foreign kinds" in the NIP-29
crate:

> 'foreign kinds'???? there are no "foreign" kinds... where the he[ll]

and, on whether the term could at least stay documented:

> that's wrong; it shouldn't be there -- there should be no "repo jargon"
> solidyfing a wrong term.

---

## 1. The rule — POLICY

Two halves:

1. **Do not invent a category that does not exist in the protocol.** If Nostr
   has no notion of a "foreign" kind, no NMP name, comment, test, or script
   may speak as if it does. A name in this repo is a claim about the protocol;
   a claimed category that isn't there is a false claim, repeated every time
   the name is read.
2. **Do not let internal shorthand harden into vocabulary.** A term coined in
   one comment for local convenience becomes "repo jargon" the moment a second
   site uses it, and jargon teaches every future reader — human or agent — a
   model of the protocol that is wrong.

The fix for a wrong term is **deletion, not a synonym**. Replacing "foreign
kind" with some other adjective would have preserved the invented category
under a new name. There was no category; the sentence is rewritten so it no
longer needs one.

## 2. The worked example: "foreign kind" — CLOSED (PR #960)

NIP-29 does not own event schemas at all — any kind can carry an `h` tag and
live in a group (see `docs/internals/nip29/group-publication.md` §4). So
there was nothing for a schema to be "foreign" *to*. The adjective was the
bug.

**What happened, concretely:** 13 sites across
`crates/nmp-nip29/src/{lib,publication}.rs` and
`scripts/check-nip29-ownership.sh` carried the term — doc headers, function
docs, fixture strings, a test name, and the ownership gate's greps. All fixed
and merged as PR #960 (master `b99f9d41`). Verified on this tree: a grep for
`foreign` over those two paths now returns **nothing**. The test is now
`draft_kind_and_schema_survive_except_for_appended_h`
(`crates/nmp-nip29/src/publication.rs:98`), and the gate's `grep -qF`
references that exact name (`scripts/check-nip29-ownership.sh:59`) — test and
gate changed together, because the gate pins the test's name literally.

**What was deliberately NOT touched, and why:**

- Roughly 85 uses of "foreign callback" / "foreign language" / "foreign
  thread" across the FFI layer — standard, correct FFI vocabulary describing
  the host-language side of the boundary. Not this defect; deleting those
  would have been a different naming error.

The scoping is part of the rule: a terminology purge is a precision
instrument, not a repo-wide `sed`.

## 3. The general lesson — POLICY

**A wrong term becomes load-bearing when a CI gate greps for it.** The
ownership gate `grep -qF`'d the test name
`foreign_kind_and_schema_survive_except_for_appended_h`, which meant the wrong
word could no longer be fixed in one file — the test and the gate had to move
in the same change or the build broke. That is the mildest version of the
mechanism; left longer, the term would have reached exported symbols, platform
bindings, and snapshots, each one another site the fix
must touch atomically.

So: **terminology defects get more expensive the longer they live.** The
moment a name is recognized as claiming something false about the protocol,
the correct time to delete it is now, in one change, old spelling gone
(`docs/internals/conventions/no-backwards-compatibility.md`) — before any
more machinery greps for it.
