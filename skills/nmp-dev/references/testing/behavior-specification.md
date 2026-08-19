# Behavioral specification

Feature files preserve product meaning across rewrites. They are not a test
inventory or project plan.

## What belongs in a feature file

Add or change a scenario when it records:

- a user correction that changes an observable result;
- a rule about routing, identity, durability, or truthful status;
- a guarantee that something must not happen; or
- a distinction that a later rewrite could easily erase.

Do not put local parser, codec, private-state, or exhaustive cases in feature
files unless an app-visible distinction would otherwise be lost.

## Scope

BDD is for durable user-visible contracts and multi-step lifecycle
guarantees — not every internal map transition. If a rewrite could replace
the internal mechanism without an app noticing, it does not need a scenario;
prove it as an owner test or headless engine scenario instead (see
[`test-placement.md`](test-placement.md)).

A test behind a scenario uses protocol-real events and deterministic
infrastructure: scripted relays, signers, and clocks. Never live internet
relays or uncontrolled data.

## Show exactly what changed

Describe the two situations that were incorrectly treated as the same. Explain
how their results should differ, what a caller can observe, and which nearby
behavior should remain unchanged. Use the smallest examples that would fail if
the situations were treated as the same again.

Organize scenarios by user behavior, never by crate. Use one promise per
scenario. Describe results with app and Nostr terms, not private Rust types,
tables, reducers, helpers, or crate names.

## Set up inputs, not answers

Setup must provide real inputs, not the result under proof. For discovery, seed
the protocol fact where discovery starts; do not insert the route that NMP is
supposed to discover. Observe the result through the public `nmp` API or a
separate witness such as a relay log.

Ask: **is this input a cause, or the conclusion being proved?**

## Correct existing behavior

When a correction conflicts with existing feature text:

1. Correct wrong text in place.
2. Split situations that should produce different results.
3. Add the example that demonstrates the difference.
4. Delete the claim the correction replaced.

Do not leave the old claim intact in an appendix or second feature.

## Quality check

A scenario must answer:

1. Which context matters?
2. What happens?
3. What observable result follows?
4. Which tempting wrong interpretation does it exclude?
