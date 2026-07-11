# Authoring a protocol module

This chapter is for library authors adding NIP-aware functionality, not for
ordinary apps using NMP.

## Claim exact protocol ownership

A module declares only the exact schemas and kinds its protocol defines. It
may own:

- typed event builders and parsers;
- tag validation and canonical encoding;
- multi-event state reconstruction;
- closed reusable demand fragments;
- typed protocol queries and semantic operations;
- protocol-defined source/access/routing context; and
- bounded use of signer, AUTH, encrypt, or decrypt capabilities.

Sparse NIP kind sets remain sparse. A convenience range is not ownership.
Ownership collisions are errors.

## Do not claim participating content

Protocol context is different from schema ownership. NIP-29 may contextualize
an article, photo, podcast episode, or app-owned event for a group. It owns only
the NIP-29 fields and authority it contributes.

The originating module remains the draft's schema owner.

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

## Make authority unforgeable

A protocol-host or private-inbox authority cannot be a public constructor over
arbitrary relay URLs. The module mints an opaque value only after validating the
protocol reference and its source facts.

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

## Package without registration framework

Module presence is a build/dependency choice. Enabling it must not require app
startup registration, a global module container, navigation hooks, or scene
callbacks.

Rust crates/features and SwiftPM/Gradle products may differ mechanically, but
they project one semantic module over the canonical facade. Disabling the module
removes its code and semantic API without changing core behavior.

## Required falsifiers

A module is ready only when tests prove:

- it cannot claim an unowned schema;
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
