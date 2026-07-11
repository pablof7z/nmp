# Extending NMP with protocol modules

**Status: TARGET.** The generic engine and closed query grammar are built. The
cross-platform protocol-module mechanism is not. Its exact package names and
registration syntax remain provisional; the ownership rules below are the
agreed contract.

## Extend protocol knowledge, not app architecture

An app enables a protocol module to avoid hand-writing event encoding,
validation, reconstruction, and routing rules from a NIP. Enabling one must not
turn the app into an NMP application or require a container, store, reducer,
scene hook, or navigation model.

The app still uses one engine facade and the same two primary operations:
observe declared data and publish an intent. Modules add typed values and
semantic operations around those operations.

## What a module owns

A protocol module may own:

- event schemas and kinds defined by that protocol;
- tag construction and validation defined by the protocol;
- parsing and typed semantic values;
- multi-event state reconstruction required for correctness;
- declarative query helpers whose full demand remains introspectable;
- semantic write operations;
- typed source/routing context defined by the protocol;
- protocol-specific capability use.

It does not own unrelated content kinds merely because those kinds can
participate in the protocol.

## NIP-29 as the boundary test

A NIP-29 module owns NIP-29 management and group-state events and knows the
group host relay. It may expose operations such as creating a group or changing
an administrator because those are protocol state transitions.

It may also bind a foreign unsigned draft to a group:

```text
group.publish(photoDraft)
```

The photo's schema owner remains the photo module. NIP-29 contributes only its
group context, including the correct `h` tag and the host-relay authority. The
core freezes the resulting draft and signs once.

This is why "every module member must desugar to a standalone filter or raw
write intent" is too narrow. A semantic operation may coordinate protocol
facts while still producing closed, inspectable engine inputs.

## Composition contract

Module composition must satisfy all of these:

1. Drafts are immutable values.
2. Each module can add only fields/context it owns.
3. Signed events are never rewritten.
4. The final signer is selected by core: current-pubkey signer by default,
   explicit identity override when requested.
5. Demand, routing, and capability selection remain closed and explainable.
6. Failures remain separated by owner. A Blossom upload failure is not a Nostr
   publish failure, and neither is reported as the other.

## What a module may not do

- Register app closures as lane mappers, comparators, or admission predicates.
- Own app state, view lifecycle, navigation, or presentation.
- Hide which protocol context changed a draft or where demand is sourced.
- Become a required import for apps that do not use the protocol.
- Add a favored content-kind path to core.
- Keep a second event store, relay pool, outbox, or signing path beside core.

## Packaging

Protocol functionality is opt-in code weight. Rust may use separate crates or
features; Swift and Kotlin should expose corresponding optional products. All
of them must call the same canonical Rust facade used by core and FFI. A direct
Rust app is not expected to assemble mechanism crates into a second supported
engine.

The exact Cargo/SwiftPM/Gradle packaging is deliberately not frozen. A proposed
shape must show binary modularity and cross-platform semantic parity before it
becomes public API.

## When the grammar itself must change

A protocol module cannot hide an opaque extension inside the query grammar. If
a real protocol cannot express its demand using the closed binding and selector
vocabulary, that is an explicit public-surface design event. The proposal must
show the missing protocol fact, its hashing/equality/routing semantics,
diagnostic representation, and projections across Rust, FFI, Swift, and
Kotlin.

---

<!-- nav-footer -->
<sub>← [Example gallery](31-gallery.md) · [Index](README.md) · [Versioning & stability](33-versioning.md) →</sub>
