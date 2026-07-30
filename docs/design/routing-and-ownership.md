---
title: Routing facts and protocol ownership
status: built
date: 2026-07-29
issues:
  - 870
---

# Routing facts and protocol ownership

NMP keeps the generic engine kind-agnostic without making routing
fact-agnostic. Core consumes a small neutral fact model; protocol crates own
the queries, parsing, winner rules, and settlement that produce those facts.

## Neutral router surface

The router reads `RoutingFacts`:

```rust
fn author_routes(&self, author: &PublicKey) -> AuthorRouteState;
fn operator_app_relays(&self) -> Vec<RelayUrl>;
fn operator_fallback_relays(&self) -> Vec<RelayUrl>;
```

`AuthorRouteState` is exactly `Unknown | Present(AuthorRoutes) | Absent`.
`AuthorRoutes` atomically owns both `outbound` and `inbound`. Internal keys are
decoded `PublicKey` values; hex exists only in NIP-01 filter serialization and
human boundaries.

The closed lane vocabulary describes neutral provenance:

- `AuthorOutbound`
- `Hint`
- `Provenance`
- `OperatorApp`
- `OperatorFallback`
- `Exact`

The route classes are `Coverage`, `Supplemental`, and `Exact`. There is no
discovery-kind classification, indexer lane, protocol marker, pinned-directory
lookup, or protocol-specific relay-list accessor in `nmp-router`.

`SourceAuthority::Pinned` and `WriteRouting::Explicit` remain because exact
destinations are ordinary query/write facts, not NIP-65 concepts.

## Mutation ownership

Production `EngineCore` owns one concrete in-memory fact store. The author
mutation capability is borrowed and non-cloneable. It accepts only a complete
`Present { outbound, inbound }` replacement or `Absent`; it cannot mint
`Unknown`. The same reducer turn replaces the fact, recompiles reads, and
rewrites open `Auto` writes.

There is no public mutable directory and no generic production injection
point. Static `FixtureRoutingFacts` snapshots and explicitly named hidden
constructors exist only for deterministic headless falsifiers.

## Protocol boundary

A protocol component:

1. receives generic author-route provider needs;
2. expresses its fetch as an ordinary exact `Demand`;
3. observes ordinary rows and `RequestSettled` evidence;
4. applies its own winner, parse, admission, and all-source rules;
5. returns neutral `Present` or `Absent` updates through the private writer.

The component never teaches the router its event kind or marker vocabulary.
The router never calls a protocol parser.

`nmp-nip65` is the first implementation of this shape. It is engine-free and
depends only on `nostr` plus `nmp-grammar`. The optional `nmp/nip65` feature
assembles it at runtime. Feature-disabled `nmp` has no normal dependency on
the protocol crate.

## Settlement

Generic request completion is `RequestSettled`, not `RelayEose`.

- Ordinary NIP-01 emits `Eose` only for the exact accepted request after its
  request-scoped facts-before-claims transaction succeeds.
- Successful NIP-77 emits `Nip77` after reconciliation, deferred until the
  missing-id backfill EOSE when necessary, and likewise only after the
  correlated request's facts-before-claims transaction succeeds.
- Refusal, close, disconnect, timeout, and abandonment settle nothing.
- A failed EVENT or coverage commit retires the terminal request without
  `RequestSettled`; local data loss can therefore never become protocol
  absence evidence.

A protocol coordinator must match the current query revision and every exact
source before deriving absence. Its winner follows the authoritative current
row projection: removing the current row clears that winner, while a
predecessor revealed in the same delta replaces it without a transient
absence. A later positive record replaces absence. Absence is memory-only and
returns to `Unknown` after restart.

## Architecture consequences

- Adding a protocol never adds a router lane, route kind, directory accessor,
  or core event-kind branch.
- One property has one owner: both route directions replace together.
- No caller can install half a fact or persist session-derived absence.
- Core-only builds can exclude protocol dependencies and symbols.
- Native protocol publication remains separate packaging work; the Rust
  boundary does not justify bundling protocol symbols into the core artifacts.
