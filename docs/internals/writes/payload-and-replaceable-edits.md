---
title: Write payloads and replaceable edits
category: writes
slug: payload-and-replaceable-edits
status: designed
date: 2026-07-29
owns:
  - the three-variant `WritePayload` and the deletion of `Unsigned`
  - how `ReplaceableEdit` improves under acceptance-time stamping
  - the CAS coordinate and where monotonicity lives
  - which compose-time errors die, and why their deaths are structural
  - the FFI invisibility of replaceable edits, and the fused-method precedent it sets
related:
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
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
  - "#47 identity override — resolution feeds the CAS coordinate"
  - "#838 deleted the second publish path; the fused-method precedent cited here is its descendant"
  - "#591 correlation tokens — orthogonal to payload shape, unchanged by this design"
---

# Write payloads and replaceable edits

This document records the designed `WritePayload` — the complete set of ways
an app can hand NMP an event to publish — settled in the 2026-07-28/29 design
session with Pablo (repository owner). Its center of gravity is §3–§4:
replaceable edits are the one place where replacing `UnsignedEvent` with the
builder does not merely simplify but makes the mechanism *more* correct, by
moving timestamp monotonicity to the only component that can actually
guarantee it.

`status: designed`. §1 and the verification anchors throughout are BUILT;
the payload set itself is not yet on master.

---

## 1. The payload set today — BUILT

`WritePayload` (`crates/nmp-grammar/src/write.rs:34-52`) has three variants:

- `Unsigned(UnsignedEvent)` — the template path; engine freezes and signs at
  acceptance.
- `UnsignedReplaceableEdit { unsigned: UnsignedEvent, expected_base:
  Option<EventId> }` — an unsigned whole-value replacement "whose acceptance
  is conditional on the store still holding exactly `expected_base` at the
  draft's replaceable/addressable coordinate. `None` means 'there is still no
  local winner'; it never means that Nostr is globally empty" (the variant's
  own doc). The precondition travels with the draft so a protocol module can
  compose one closed, race-free write value, and is checked inside the
  store's atomic acceptance transaction.
- `Signed(SignedEvent)` — a caller-held signed event; verified verbatim at
  acceptance (`crates/nmp/src/core/write.rs:1455-1462`), never re-signed,
  routed as-is.

## 2. The designed set — DESIGNED

```rust
pub enum WritePayload {
    Event(EventBuilder),
    ReplaceableEdit {
        builder: EventBuilder,
        expected_base: Option<EventId>,
    },
    Signed(Event),
}
```

`Unsigned` is **deleted** — same change, no alias, per
`conventions/no-backwards-compatibility.md`. The case for its death is made
in `writes/event-builder.md` §6: with the deterministic-bytes requirement
killed ("we don't need that shit at all"), nothing `Unsigned` can express
remains that the builder or the pre-signed path cannot. The word "unsigned"
disappears from the payload vocabulary along with the type.

**Correction, recorded while building this (#973/PR #1005).** This section
originally said `nostr::UnsignedEvent` leaves the facade's re-export list
because "its only public job was being `Unsigned`'s argument". That was
wrong: `Handle::sign_event(UnsignedEvent)` (#464) is a separate public door
whose parameter must be nameable, so the re-export stays and only the payload
variant dies. See `writes/event-builder.md` §5 for the full note. Nothing
forwards to a deleted spelling, so no-backwards-compatibility is untouched.

The three variants map exactly onto the three possible author sources
(`writes/identity.md` §4): `Event` has no author until identity resolution
stamps one; `ReplaceableEdit` likewise, plus a precondition; `Signed` carries
its author in its bytes. There is no fourth place an author could come from,
which is the informal argument that this set is complete.

`ReplaceableEdit` keeps `expected_base: Option<EventId>` with its exact
current meaning, including the deliberate asymmetry of `None`: it asserts "no
*local* winner", never "Nostr is globally empty" — first-write-wins races
against the network remain possible and are the protocol's nature, not a gap
in the CAS. What the CAS guarantees is that the *local* store never silently
clobbers a base the composer didn't see.

## 3. Replaceable semantic operations remove the stale-base seam — BUILT

A replaceable event must outrank the value it replaces. A caller-side
read/compose/publish loop cannot choose that timestamp reliably because a
newer source can arrive between any two steps.

`ReplaceableOperation` moves the operation, not a prebuilt event, into durable
custody. The registered capability receives NMP's current source and current
pending generation, returns a complete `EventBuilder`, and leaves timestamp
authority with the engine. NIP-02 therefore records “follow Alice” rather than
“publish this kind:3 built from event X”.

When no source exists, only a capability that implements
`materialize_default` can produce a first value. NIP-02 supplies exactly one
complete empty kind:3 for the frozen author and then applies its versioned
operation. This is a capability policy, not a cache-miss or relay-absence
claim.

When a newer relay event arrives, the engine invokes the same materializer
over that source, preserves the same durable operation/receipt identity, and
publishes a successor generation. No app-side retry loop or duplicated
timestamp/error vocabulary exists. Future protocol capabilities inherit this
ownership shape while still defining their own source requirement,
first-value policy, and preservation rules.

## 4. The CAS coordinate, and the foreign-base failure — DESIGNED

Today the store derives the CAS coordinate from the event itself
(`crates/nmp-store/src/address_key.rs`): `(event.pubkey, kind)` for
replaceable kinds, `(event.pubkey, kind, d)` for addressable ones. The
precondition check runs inside `accept_write`'s atomic transaction
(`crates/nmp-store/src/redb_store/write_ops.rs`): look up the canonical winner
at that coordinate and return
`AcceptOutcome::Refused(RefuseReason::ReplaceableBaseChanged { expected,
actual })` if it is not `expected_base`. That transaction creates no accepted
intent, journal row, optimistic event, or receipt id for a journaled intent.

The semantic answer is nevertheless in custody. `EngineCore` immediately
passes it through `accept_refused`, which allocates one durable receipt id and
stores one terminal receipt-only record (`intent_id: None`) carrying the
frozen attempted event id and the typed expected/actual pair. The receipt
stream ends with
`WriteOutcome::Refused(RefuseReason::ReplaceableBaseChanged { expected,
actual })`; no signer request, route, lane, attempt, relay write, or retry
obligation exists. The app can reattach or enumerate that receipt, fetch the
actual winner, reapply the user's change, and resubmit. The executable anchors
are `replaceable_base_precondition_rejects_a_concurrent_winner_atomically`,
`a_refused_write_is_taken_into_custody_as_one_permanently_failed_receipt`, and
`stale_replaceable_edit_is_refused_into_custody_keeping_both_event_ids`.

Under the builder, the event has no `pubkey` until identity resolution
supplies one, so the coordinate becomes **`(kind, resolved_identity, d)`** —
the same key, sourced from `Identity` resolution (`writes/identity.md`)
instead of from a caller-stamped field. Identity resolution already runs
before acceptance commits (#47's pin), so the coordinate is fully determined
inside the same transaction that checks the precondition and (per §3) stamps
the timestamp. One transaction, one row, three formerly-scattered concerns.

**The foreign-authored base needs no dedicated error.** Suppose an app passes
`expected_base = Some(id_of_someone_elses_contact_list)` — the case
`BaseHasWrongAuthor` used to catch at compose time. The CAS runs at the
*resolved identity's* coordinate; a foreign-authored event is never the
canonical winner at your coordinate; so `actual != expected` and the write
fails the precondition with the existing typed
`WriteOutcome::Refused(RefuseReason::ReplaceableBaseChanged { expected,
actual })`. No new variant, no author comparison — the coordinate system
itself makes a foreign base unsatisfiable, and reports it through the same
door as every other stale base. (The store also fails closed on a
precondition attached to a kind with no replaceable coordinate at all:
`RefuseReason::ReplaceableBaseOnRegularEvent`. That guard
survives unchanged.)

One acceptance rule carries over verbatim: replaceable edits refuse
`Durability::Ephemeral` ("replaceable edits require durable or at-most-once
acceptance", `write.rs:1580-1585`) — a CAS against the durable canonical
store from a write that refuses to be journaled is incoherent, and stays
refused.

## 5. Replaceable edits are already invisible to FFI — BUILT, and it is a precedent

This section records a fact that corrected an earlier assumption in the
design exploration (which had listed "a builder-only payload must say what
happens to `UnsignedReplaceableEdit` at FFI" as an open problem). The problem
does not exist, because the variant never crossed the boundary in the first
place. Verified on master:

- `FfiWritePayload` (`crates/nmp-ffi/src/types.rs:584-601`) has exactly two
  variants: `Unsigned` and `Signed`. There is no
  `UnsignedReplaceableEdit` mirror and never was.
- Replaceable edits cross the boundary solely as **fused semantic methods**:
  `NmpEngine::follow` / `NmpEngine::unfollow`
  (`crates/nmp-ffi/src/facade.rs`). They submit a registered semantic
  operation, materialize it over the best current source or NIP-02's complete
  empty first value, retain it for later-source replay, and project the
  ordinary receipt. The native button owns none of those steps.

So the boundary rule — **a payload never crosses FFI; only a fused semantic
method does** — is an EXISTING precedent, not something this design invents.
The Swift/Kotlin surface never learns a source id, operation encoding, or
materialization callback; it learns `follow(target)`. `FfiWritePayload`
remains `{ Event, Signed }`, and semantic replacement remains representable
only through doors that own its policy.

The precedent is what justifies the NIP-29 group-publish projection by
analogy (`nip29/group-publication.md`): Pablo's ruling —

> about nip29: sure, at the gate level, but for the app this needs to happen automatically; i.e. the app shouldn't say "publish to group x, relay y", it should create a "group" object like we've seen before and group.publish(event_builder_stuff) would take care of adding the h and publishing to the correct relay.

— has the same shape as `follow`/`unfollow`: the raw capability (an
`Explicit` route to a host relay; a CAS-guarded whole-value replacement)
exists in the Rust grammar for composers to use, while the app-facing
boundary exports the *workflow*, fused, with its policy applied. Where the
raw shape earns app-facing exposure on its own merits (as `Explicit` routing
does — see `routing/auto-and-explicit.md`), it gets it; where it is only ever
correct inside a workflow, only the workflow crosses.

The failure mode both instances guard against is the same: a boundary that
exports the pieces instead of the workflow makes every native app reassemble
the workflow, and each reassembly is a chance to skip the evidence policy,
drop the precondition, or stamp the wrong author — the exact regression class
#838 closed by deleting the second publish path.

## 6. What to watch when building this — DESIGNED

- **The stamp and the CAS must stay in one transaction.** §3's entire
  argument is that the timestamp is computed against the row being CAS-ed.
  An implementation that stamps in the reducer and CAS-es in the store
  reintroduces the seam at a smaller scale — the winner could move between
  stamp and check. The `max(clock, winner + 1)` read belongs inside
  `accept_write`'s transaction, next to the `expected_base` comparison it
  shares a row with.
- **An explicit `created_at` on a `ReplaceableEdit` builder is a foot-gun to
  leave loaded.** Ruling 6 says the builder can provide ANYTHING, and that
  holds here too — but a caller-stamped timestamp on a replaceable edit can
  regress below the winner and lose the replacement race by design. The
  engine must not silently "fix" it (present-then-changed stays impossible,
  `writes/event-builder.md` §3); guardrail-versus-restriction policy says let
  it through and keep the failure observable, not forbidden.
- **NIP-02 migration.** The exact-base composer and its acquisition/retry
  action are deleted. `FollowWrites` mints a versioned operation through the
  compiled program/format supplied at engine construction. The engine
  materializes it against the current source, preserves unowned fields, and
  automatically reapplies it when a newer source wins.
- **Receipts.** Successful follow/unfollow exposes the ordinary receipt
  directly. Successor generations remain attached to the same receipt; there
  is no second action status machine or stale-base retry signal.
