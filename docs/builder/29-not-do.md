# What NMP does not own

**Status: ARCHITECTURE CONTRACT.** Public spellings remain provisional, but the
ownership boundary is the thesis being falsified.

NMP owns generic Nostr sync, store, routing, signing orchestration, durable
publication, and protocol-semantic modules. It does not own the app around
those mechanisms.

## It does not own app architecture

There is no NMP state container, reducer model, provider hierarchy, navigation
system, or scene-phase lifecycle to adopt. An app constructs an engine, opens
queries, publishes intents, observes receipts/diagnostics, and folds values into
its own state.

Query lifetime follows ordinary native ownership. Enabling an opt-in protocol
package must not require module registration in an app container or create a
second engine assembly path.

## It does not own presentation or product policy

NMP returns raw protocol-semantic values. The app owns:

- formatting, display names, avatars, and localization;
- which queries exist and how several query results form a product surface;
- ranking, ordering beyond closed engine cursor needs, and moderation UX;
- what counts as a feed, conversation, notification, or empty-state policy;
- navigation, rendering, and observation scope.

A protocol module may define NIP semantics such as membership, an admin action,
or a typed event builder. It may not turn a popular app convention into a core
default. Core ships no privileged kind:1 helper catalog or preferred timeline.

## It does not accept generic relay overrides

Raw observe/publish calls do not accept an app-computed relay array. NMP derives
routes from typed source, lane, ownership, provenance, and operator facts.

This does not forbid protocol authority. A typed NIP-29 operation may carry the
group's host relay because that relay is part of the protocol context being
validated. The contribution is closed, inspectable, and diagnostic-visible; it
is not a generic "send this anywhere" escape hatch.

## It does not make one identity globally active for every purpose

`$currentPubkey` is a reactive input and the default signer identity for the
common publish path. It is not a command to clear all other account demand,
partition the cache, or change a signer already pinned to an accepted write.

Literal multi-account queries may remain live. Exceptional writes may select a
different registered identity without changing `$currentPubkey`. AUTH and
crypto choose the capability required for their operation.

The app owns account list, login/logout UX, labels, import/removal/backup
policy, and whether to attach a custom provider. Platform SDKs should provide
standard secure signer providers so ordinary apps do not hand-roll vault
plumbing.

## It does not treat account switching as a privacy boundary

One engine is one local trust domain with one canonical cache. Valid public
events are shared across matching queries regardless of which identity caused
their acquisition. AUTH context remains attributable because it changes source
evidence, not because local rows are hidden after validation.

An app serving mutually untrusted local users needs an explicit destructive
reset that clears cache, pending writes, receipts, evidence, and capabilities.
Changing `$currentPubkey` is not logout-and-wipe.

## It does not store raw secrets in the event/outbox database

NMP persists frozen bodies, expected authors, signatures, pending obligations,
attempts, and receipts. Secret material belongs behind a signer provider and
platform secure storage. NMP coordinates capability use; it is not a universal
cross-platform key vault.

## It does not infer global Nostr truth

EOSE, watermarks, connection state, AUTH, and errors are facts about planned
sources. NMP does not emit `synced`, `syncHealth`, global `complete`, or
`authoritativeEmpty`. Apps interpret cache plus source evidence for their own
UX.

## Protocol features return as opt-in modules, not core bias

NIP-29 groups, NIP-68 media events, NIP-17 messaging, Blossom integration, and
other protocol capabilities may each need typed builders, parsers, queries,
state machines, or semantic operations. They belong in opt-in modules that own
only their exact protocol surface.

The question is not whether a protocol is popular or content-shaped. The tests
are exact schema ownership, deterministic immutable composition, typed bounded
capability access, no app-framework lifecycle, and no hidden subscription or
route path.

---

<!-- nav-footer -->
<sub>← [Guarantees and bug classes](28-patterns.md) · [Index](README.md) · [Platform SDK guides](30-platform-guides.md) →</sub>
