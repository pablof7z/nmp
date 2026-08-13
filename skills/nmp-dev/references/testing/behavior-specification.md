# Behavioral specification

Feature files preserve product meaning across rewrites. They are not a test
inventory or project plan.

## Admit behavior deliberately

Add or change a scenario for a product-meaningful:

- user correction or contextual distinction;
- routing, identity, durability, or truthfulness rule;
- negative guarantee or counterexample;
- boundary easy to flatten during later work.

Keep local parser, codec, private-state, and exhaustive cases in their owning
tests unless they express such a boundary.

## Preserve the semantic delta

State: which cases were conflated, which axis changes the result, what remains
unchanged, and the observable consequence. Use the smallest contrasting
examples that would fail if the cases were conflated again.

Organize by behavioral domain, never crate. Use one promise per scenario.
Prefer app and Nostr terms; avoid private Rust types, tables, reducers, helpers,
and crate ownership as outcomes.

## Stage causes

Setup must provide real inputs, not the result under proof. For discovery, seed
the protocol fact at the starting source; do not inject the resolved route.
Observe through the supported facade or an independent witness.

Ask: **is this input a cause, or the conclusion being proved?**

## Metadata

Place one adjacent comment block above each governed scenario:

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
| `built` | Evidence line(s) and one falsifier |
| `specified` | `nmp:gap=implementation|evidence|fixture|platform`, open `nmp:issue=#N` |
| `known-violation` | Open `nmp:issue=#N` |

`@acceptance` selects a built facade capstone; it is not status. Governed files
reject `@wip`, `@designed`, and `@requires-*` lifecycle tags.

Keep an ID while refining one promise. Split IDs when one scenario contains
several promises. Never reuse a deleted ID.

Once one scenario in a file has `nmp:*`, govern the whole file. Govern legacy
before later deleting it.

## Correct existing behavior

When a correction conflicts with the corpus:

1. Correct wrong text in place.
2. Split conflated contexts.
3. Add the missing contrast.
4. Update status, evidence, falsifier, and issue.

Do not leave the old claim intact in an appendix or second feature.

## Quality check

A scenario must answer:

1. Which context matters?
2. What happens?
3. What observable result follows?
4. Which tempting wrong interpretation does it exclude?
