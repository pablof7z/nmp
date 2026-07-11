# Editing replaceable state safely

**Status: OPEN DESIGN.** The store correctly applies replaceable-event winner
semantics. NMP does not yet provide a structural safe-edit operation, and the
previous proposal relied on aggregate `CompleteUpTo` as proof of a globally
current base. That proof is not available on Nostr and is no longer the target
contract.

## The destructive-write trap

Profiles, contact lists, relay lists, and parameterized replaceable events are
whole-value replacements. A client that reads an empty or stale local cache,
constructs a replacement from it, and publishes can permanently erase fields
that existed elsewhere.

The canonical failure is simple:

1. The local store has not acquired the current contact list from all relevant
   sources.
2. The user adds one contact.
3. The app treats local absence as an empty list and publishes a one-entry
   replacement.
4. The newer event wins at relays and the previous list is lost.

## What current NMP can say

NMP can return:

- the current local winning row under replaceable semantics;
- cache evidence for that row;
- per-source acquisition evidence for the current plan;
- exact relay/watermark facts through diagnostics.

It cannot prove that no newer or different winner exists on an unknown relay.
EOSE from every currently planned source is useful evidence, not global
authority.

Therefore this is not a valid structural contract:

```text
CompleteUpTo -> safe to edit globally
```

## What apps should do today

Until a mechanism is designed and implemented:

- never construct destructive replacement state from a bare cache miss;
- inspect the query's source evidence and apply an explicit product policy;
- preserve every field/tag from the local winner that the operation does not
  intentionally change;
- publish durably and retain the receipt;
- make uncertainty visible rather than presenting the mutation as guaranteed.

This is risk reduction, not a type-level guarantee. The manual must not claim
that app discipline makes the bug unrepresentable.

## The unresolved mechanism

A future protocol-aware edit operation likely needs to state its base event and
the source evidence under which it was accepted, then fail typed if the local
base changes before acceptance. That can prevent races against the canonical
store and make the app's acquisition assumptions explicit.

It still cannot manufacture global completeness. The design must decide:

- which source plan and evidence a destructive operation requires;
- whether the protocol module or app selects that policy;
- how concurrent relay winners are reconciled;
- what happens offline or with an AUTH-blocked source;
- how retry interacts with a base that becomes stale after acceptance.

Do not add a blessed one-call mutation such as `follow()` until that contract is
settled. A protocol module may own the NIP-defined event schema and validation;
it must not hide unresolved destructive-state policy behind convenience.

---

<!-- nav-footer -->
<sub>← [Writing: accepted intent, local state, and relay evidence](14-writing.md) · [Index](README.md) · [Identity](16-identity.md) →</sub>
