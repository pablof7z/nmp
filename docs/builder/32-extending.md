# Authoring a protocol module

This chapter is for library authors adding NIP-aware functionality, not for
ordinary apps using NMP.

## Own exactly the schemas your protocol defines

A module implements only the exact schemas and kinds its protocol defines —
"owns" here means "is the crate that defines, builds, and parses it", which is a
fact about the code and its dependents, not a declaration made to a registry. It
may own:

- typed event builders and parsers;
- tag validation and canonical encoding;
- multi-event state reconstruction;
- closed reusable demand fragments;
- typed protocol queries and semantic operations;
- protocol-defined source/access/routing context; and
- bounded use of signer, AUTH, encrypt, or decrypt capabilities.

Sparse NIP kind sets remain sparse. A convenience range is not ownership.

Two crates parsing the same numeric kind is not, by itself, a collision — the
runtime authority boundary is the complete operation, not global ownership of a
number. The collision worth catching in review is two public operations claiming
the same semantic responsibility, or generic core code branching on protocol
meaning.

## Do not claim participating content

Protocol context is different from schema ownership. NIP-29 may contextualize
an article, photo, podcast episode, or app-owned event for a group. It owns only
the NIP-29 fields and authority it contributes.

The originating module remains the draft's schema owner.

Dependencies do not transfer ownership either. An NIP-29 module may depend on
NIP-51 and compose typed kind `10009` Simple-group entries into NIP-29 group
references. NIP-51 remains unaware of NIP-29 and exclusively owns the `10009`
schema; NIP-29 claims neither `10009` nor generic kind `30002` relay sets.

## Return closed values

Module APIs may return:

- public `Filter`/`Binding` graphs;
- typed values reconstructed from ordinary live demands;
- immutable unsigned drafts;
- opaque validated source/access/routing authority; or
- semantic operations that resolve to core demands/write intents.

They may not register callbacks that later decide demand, routing, signer
selection, ordering, or admission.

If a module needs a new grammar node, propose a public closed vocabulary change
with defined hashing, equality, persistence, diagnostics, and Rust/Swift/Kotlin
projection. Do not hide the missing concept in an opaque extension payload.

## Keep engine-free composition off Engine

Needing the engine and eventually publishing through the engine are different
facts. If a protocol operation can compose an existing `Demand` or
`WriteIntent` entirely from explicit inputs, expose it as a protocol-owned free
function. Do not add it to `Engine` for naming symmetry, discoverability, or
because another protocol currently has an engine-bound operation.

NIP-22 is the reference shape. Comment composition takes the root, parent,
author, timestamp, content, and optional correlation; it returns the ordinary
write intent. Swift and Kotlin use the matching top-level `commentIntent`
function, then pass that value to generic `publish`. No `CommentIntent`
wrapper, take-once lifecycle, or NIP-22 `publishComposed` overload exists.

## Distinguish public protocol context from private authority

A public protocol may make one host relay part of an object's identity. An
NIP-29 composer can therefore accept `(groupId, hostRelay)`, but the resulting
host authority must be non-forgeable and payload-bound inside the ordinary
immutable `WriteIntent`. It is not a generic relay list and cannot be extracted
or reused to route unrelated traffic. The current parallel NIP-29 intent and
publication lifecycle does not satisfy this architecture; #838 supersedes
#823 and owns the protocol-boundary correction and removal.

Private-inbox or recipient authority is stricter: it cannot be a public
constructor over arbitrary relay URLs. The owning module or engine mints it only
from verified recipient/source facts.

Core can inspect the value's reason and relay constraints without giving app
code a raw widen operation. Diagnostics shows the module/context that produced
the lane.

## Compose drafts immutably

Every stage returns a new unsigned value and contributes only fields it owns.
The operation fails before acceptance if contributions conflict or violate a
narrow/private route.

No module may:

- mutate a signed event;
- sign early;
- access raw signer secret material;
- write directly to store indexes;
- publish through its own transport; or
- maintain a second optimistic row path.

Core validates the final body/context, pins the signer, accepts the canonical
row, signs once, and routes through the durable outbox.

## Keep failure ownership separate

An upload failure, draft-validation failure, AUTH failure, signer denial,
acceptance failure, and relay rejection are different facts. A module maps only
the failures it owns and preserves core receipt/source evidence for the rest.

## Compose modules by dependency, not by registration

Module presence is a build/dependency choice, and that is the entire mechanism.
The one app/platform composition root links the modules it enables. Nothing is
passed into engine construction to announce them: there is no claim vocabulary,
no `ModuleRegistration`, no callback registry, and no global module container.
#859 deleted that design, and #757/#758 (which would have wired it into routing)
are closed NOT_PLANNED — the router must not depend on a shared protocol
vocabulary any more than it depends on concrete module crates.

Modules perform no startup work and require no navigation, scene, or application
lifecycle hooks.

Rust crates/features and SwiftPM/Gradle products may differ mechanically, but
they project one semantic module over the canonical facade. Disabling the module
removes its code and semantic API while leaving the raw core facade usable.

## Required falsifiers

A module is ready only when tests prove:

- it neither redefines nor re-parses a schema another crate already owns;
- its reusable binding prints exactly like the raw expansion;
- reconstructed state uses canonical store/query semantics;
- source/routing authority cannot be forged from app relay arrays;
- cross-module composition produces deterministic final unsigned bytes;
- core signs the composed body once;
- Swift, Kotlin, and direct Rust agree on bytes and observable facts;
- disabling it leaves the raw engine useful; and
- no hidden lifecycle, store, signer, or transport path appears.

---

<sub>[Index](README.md) · Related: [Using protocol modules](27-recipes-and-choosing.md) · [Governed provisional API](33-versioning.md)</sub>
