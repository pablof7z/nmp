# Protocol modules and contextual composition

- **Status:** TARGET CONTRACT - opt-in module packaging and contextual
  publication are not yet implemented as one cross-platform API.
- **Owns:** the content-agnostic core boundary, protocol schema ownership,
  derived helpers, immutable draft composition, and contextual routing.

## 1. Core versus module

The engine core owns universal mechanism: canonical events, demand, store,
routing, sync, signing orchestration, receipts, and diagnostics. It does not
ship a preferred timeline, kind:1 helper catalog, or blessed content model.

An opt-in protocol module may own:

- the exact event schemas and kind values defined by its NIP;
- typed builders and parsers for those schemas;
- protocol validation and state reconstruction;
- reusable derived demand fragments;
- typed protocol queries and semantic operations;
- typed routing/access facts that the protocol itself defines.

The module mechanism must be opt-in and pay-for-what-you-enable. Exact Cargo,
SwiftPM, and Kotlin packaging remains provisional; packaging cannot create a
second, less-safe way to assemble the engine.

## 2. Ownership is exact

Schema ownership is neither content-category ownership nor a blanket routing
monopoly. A module claims only the exact event schemas its protocol defines.
Broad ranges used for convenience are not acceptable when the NIP defines a
sparse set.

NIP-29 therefore owns its group metadata, administrator, membership, and
moderation event schemas. It does not own a photo, article, podcast episode, or
other foreign event kind published in a group.

Core remains ignorant of those schema meanings unless the app enables the
module. Ownership collisions are errors, but contextual use of a foreign-owned
draft is not an ownership collision.

## 3. Immutable unsigned drafts

Protocol construction composes immutable values. Each stage returns a new draft
and may contribute only fields authorized by its protocol contract. No stage
signs early, mutates a shared event behind the caller's back, or takes a closure
that later decides engine behavior.

Illustrative, deliberately non-binding syntax:

```text
asset   = Blossom.upload(file)
photo   = Nip68.buildPhoto(asset)
grouped = nip29.group(groupId, hostRelay).compose(photo)
receipt = engine.publish(grouped)
```

Responsibilities remain separate:

- Blossom uploads bytes, verifies them, and returns an asset reference.
- NIP-68 constructs the photo draft and owns that event schema.
- NIP-29 adds only group context required by NIP-29, including the correct `h`
  tag and host-relay constraint.
- Core validates the final draft, resolves the chosen signer, signs exactly
  once, persists it, and publishes it.

Current implementation status: `nmp-nipc7` ships kind:9/`q` construction
(#838), and NIP-29 group publication is a shipped API. `nip29::Group` in the
`nmp` facade mints both halves of a group's traffic -- a read `Demand` and a
complete `WriteIntent` (`h` row appended before signing, routing pinned to the
group's host) -- and hands the intent to the same `Engine::publish` door every
other write uses (#977, #1011). The illustration above is not literal API
syntax, but the composition proof it stood in for is complete.

Upload failure and Nostr publication failure are distinct results. NIP-29's
contextual publication does not transfer schema ownership of the photo to
NIP-29.

## 4. Contextual routing contribution

The routing model distinguishes:

1. **Schema-owned policy:** rules inherently attached to an event schema owned
   by the module.
2. **Context contribution:** a closed typed value attached by an operation such
   as group publication, for example `HostRelay(group, relay)`.
3. **Operator/source policy:** app configuration such as indexer lanes.

Raw arbitrary relay arrays do not become a general publish escape hatch.
Context contributions are inspectable, validated against the operation that
created them, carried into diagnostics, and combined by core policy. A module
does not register a route closure.

If contributions conflict or would violate a private/narrow route, composition
fails with a typed error before acceptance. Core signs only the validated final
body and route context.

## 5. Reusable demand without privileged content

A lightweight helper may return a public `Filter`/`Binding` graph. Its expansion
must be printable and equivalent to writing the graph directly. This is the
right shape for a commonly reused derived set.

A richer module query may return typed protocol values assembled from one or
more ordinary live demands. It still may not introduce an alternate
subscription lifecycle, cache, app callback, or hidden relay expansion.

The core documentation and acceptance suite must use kind-diverse examples.
No initial module roadmap may make kind:1 the assumed center of the product.

## 6. Facade and platform projection

Direct Rust apps and FFI use the same invariant-preserving facade. Swift and
Kotlin expose native spellings of the same draft, context, demand, and receipt
values. A protocol crate must not require an app to reach into mechanism crates
or register itself into an NMP application container.

Module operations may use public engine capabilities, but capability access is
bounded and typed. A module cannot obtain raw signer material, arbitrary store
mutation, or unrestricted routing control.

An operation that needs no engine capability does not receive an Engine
receiver for symmetry. NIP-22 comment composition is the concrete model: the
protocol owner takes explicit semantic input, author, timestamp, and optional
correlation, then returns the ordinary write intent. FFI and both native SDKs
project that as a protocol-owned free function; core publication remains the
one generic `publish` → receipt lifecycle. A separate wrapper noun or
protocol-specific publication overload would create a second owner of the
same write.

## 6.1 Both forms, and one is a wrapper over the other — OWNER RULING, 2026-08-17

A capability offers **both** the primitive that constructs the event and a
publishing convenience over it. The ruling came in two halves on the same day,
and the second half corrected an over-strong reading of the first.

The first half — the publishing form must exist, for every capability, not
just the ones that happen to have it:

> it's an implementation difference that should be fucking ironed out!
> nip25::react and all the other shit should be able to take the engine and do
> the publishing for the full experience

That was briefly taken to mean the primitive should be deleted so there is
only one lane. The second half rejects that, and names the reason:

> "Reacting inside a NIP-29 group breaks." => THAT'S TRUE -> WE NEED THESE
> EVENTS TO ALSO PROVIDE A NON-BATTERIES-INCLUDED-WE-WILL-PUBLISH-FOR-YOU. The
> publish function should be just a nice wrapper on the primitive that only
> constructs the event and returns it.

So the shape is:

- **the primitive** constructs the event and returns it, taking no engine;
- **the publishing form** is a thin wrapper over that same primitive — it adds
  the publish call and nothing else.

"And nothing else" is the load-bearing half. If the wrapper composes the event
a second way, or applies policy the primitive does not, the two forms drift
and the capability has two definitions of its own event.

Deleting the primitive was wrong because four shipping things go through it
and cannot be expressed without it:

1. **Composition inside a NIP-29 group.** `react()` produces an event; the
   group door contextualizes it with the `h` tag and the group's own route.
   With no primitive nothing reaches that door — no reacting, chatting,
   reposting or replying inside a group.
2. **Blossom kind:24242 authorizations** (`crates/nmp-blossom/src/auth.rs`)
   are signed and base64'd into an HTTP `Authorization` header. They never go
   to a relay at all: primitive only, no wrapper.
3. **Republishing someone else's already-signed event** to a personal archive
   relay — the owner's own worked example, and the reason
   `WritePayload::Signed` exists (`docs/internals/routing/auto-and-explicit.md`
   §4).
4. **`nmp-nip68::build_picture`**, which returns an unsigned event for the
   caller's own signer.

Neither form resolves the author. Both inherit the one mechanism that does —
`docs/internals/writes/identity.md` §7.

#838 removed NIP-29's former `FfiComposedWriteIntent` / `GroupSendIntent` and
`publishComposed` path and replaced it with the pure `GroupPublication`
value. #977/#1011 later deleted `GroupPublication`, `contextualize_group_event`,
and `crates/nmp-nip29/src/publication.rs` outright -- no alias, no
deprecation -- and replaced them with `nip29::Group` in the `nmp` facade: an
identity value that mints both halves of a group's traffic, a read `Demand`
and a complete `WriteIntent`, and hands the intent to the same
`Engine::publish` door every other write uses. That single-host publication
route is complete: no forgeable raw relay override, no parallel write noun,
and no second publication lifecycle.

## 7. Falsification

Required proofs include:

- enabling no protocol module retains a useful raw two-noun engine;
- a module cannot claim a kind it does not define without an ownership failure;
- NIP-29 preserves a foreign-owned draft while adding only `h` and retaining
  its selected host; the later engine publication proof must preserve that
  same ownership boundary;
- composition is deterministic and the core signs once;
- a reusable fragment prints the same graph as its raw construction;
- disabling a module removes its code and semantic API without changing core;
- Swift, Kotlin, and direct Rust produce byte-identical final unsigned bodies
  for the same composed operation;
- an engine-free module composer remains absent from every generic Engine
  facade while its protocol-owned free functions remain available; and
- no module callback or hidden subscription lifecycle enters engine decisions.
