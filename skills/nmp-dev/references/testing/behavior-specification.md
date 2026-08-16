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

Required tests behind a `built` scenario use protocol-real events and
deterministic infrastructure: scripted relays, signers, and clocks. Never
live internet relays or uncontrolled data — an opt-in live check is a
separate, supplemental locator (see
[Evidence and traceability](evidence-and-traceability.md)).

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

## Metadata

Place one adjacent comment block above each scenario that uses NMP metadata:

```gherkin
# nmp:id=ROUTING-DISCOVERY-003
# nmp:status=built
# nmp:evidence=rust:nmp::self_bootstrap_discovers_write_relay
# nmp:falsifier=Disable relay-list ingestion; the content relay is not contacted.
@acceptance
Scenario: An author relay is discovered without an app-supplied route
```

Required:

- `nmp:id=<DOMAIN>-<CONTEXT>-<NNN>`: unique and stable;
- `nmp:status=built|specified|known-violation`.

Additional fields:

| Status | Required |
|---|---|
| `built` | Evidence locator(s) and one `nmp:falsifier` |
| `specified` | `nmp:gap=implementation|evidence|fixture|platform`, open `nmp:issue=#N` |
| `known-violation` | Open `nmp:issue=#N` |

`nmp:falsifier` describes a small deliberate break that the linked evidence
must catch. See [Evidence and traceability](evidence-and-traceability.md).

`@acceptance` marks a built scenario that is also tested through the public
Rust `nmp` API. It is not a status. Files using this metadata reject `@wip`,
`@designed`, and `@requires-*` tags that try to express incomplete work in a
second way.

Keep an ID while refining one promise. Split IDs when one scenario contains
several promises. Never reuse a deleted ID.

Once one scenario in a file has `nmp:*`, every scenario in that file must have
the required metadata. Add it to older scenarios before deleting or replacing
them; do not use deletion to avoid the metadata rules.

## Correct existing behavior

When a correction conflicts with existing feature text:

1. Correct wrong text in place.
2. Split situations that should produce different results.
3. Add the example that demonstrates the difference.
4. Update status, evidence locators, `nmp:falsifier`, and issue.

Do not leave the old claim intact in an appendix or second feature.

## Quality check

A scenario must answer:

1. Which context matters?
2. What happens?
3. What observable result follows?
4. Which tempting wrong interpretation does it exclude?
