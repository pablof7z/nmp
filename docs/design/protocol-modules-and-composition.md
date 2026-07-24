# Protocol modules and contextual composition

- **Status:** TARGET CONTRACT - opt-in module packaging and contextual
  publication are not yet implemented as one governed cross-platform surface.
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
receipt = nip29.group(groupId, hostRelay).publish(photo)
```

Responsibilities remain separate:

- Blossom uploads bytes, verifies them, and returns an asset reference.
- NIP-68 constructs the photo draft and owns that event schema.
- NIP-29 adds only group context required by NIP-29, including the correct `h`
  tag and host-relay constraint.
- Core validates the final draft, resolves the chosen signer, signs exactly
  once, persists it, and publishes it.

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

## 7. Falsification

Required proofs include:

- enabling no protocol module retains a useful raw two-noun engine;
- a module cannot claim a kind it does not define without an ownership failure;
- NIP-29 can publish a foreign-owned draft with `h` and host routing while the
  foreign schema owner remains unchanged;
- composition is deterministic and the core signs once;
- a reusable fragment prints the same graph as its raw construction;
- disabling a module removes its code and semantic API without changing core;
- Swift, Kotlin, and direct Rust produce byte-identical final unsigned bodies
  for the same composed operation;
- no module callback or hidden subscription lifecycle enters engine decisions.

## 8. BUD-03 server-list placement

Blossom owns both event schemas defined by the Blossom specification:
kind:24242 authorization and kind:10063 user server lists. Kind:10063 is
therefore an exclusive `nmp-blossom` claim, not a generic `nmp-nip51` list
claim. Grouping every 1xxxx event under one module would make packaging
convenience—not protocol ownership—the authority.

Discovery stays inside the ordinary read noun:

- one `Demand` selects kind:10063 authored by the reactive active pubkey;
- its source/access defaults are `AuthorOutboxes + Public`;
- the canonical store owns replacement, deletion, expiry, and absence;
- account changes use the resolver's normal reroot/reconstruct path;
- decoding preserves signed tag order and retains malformed/content evidence;
- no Blossom-specific observation, cache, winner selector, or availability
  verdict exists.

A signed server tag is a user fact, never permission to dial a network
location. Candidate use is a separate typed qualification step. Callers choose
`SignedListOnly`, `OperatorOnly`, or `SignedListThenOperator`; the last form
keeps the signed list first as BUD-03 requires and appends configuration only
as provenance-visible fallback. Every candidate crosses the shared
`BlossomServerUrl` syntax gate plus literal-host and post-DNS SSRF admission.
Only the operator's explicit local-host allowlist can admit a local/private
answer. HTTP operations repeat admission so qualification cannot become a DNS
rebinding bypass.
