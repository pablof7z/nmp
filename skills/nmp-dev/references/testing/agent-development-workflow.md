# Agent development workflow

This workflow applies whenever an agent changes NMP behavior, fixes a bug, adds a protocol capability, or receives user nuance about how something should work.

The goal is not “add tests.” The goal is to preserve behavioral meaning and produce trustworthy evidence.

## Before editing code

1. Read [`INDEX.md`](INDEX.md).
2. Read the domain-specific guide relevant to the work.
3. Search the feature corpus for the behavior, its contrasts, and adjacent rules.
4. Search executable tests and evidence locators.
5. Read the owning architecture/design contract and any open issue.
6. Identify the narrowest stable contract that owns the guarantee.

Do not begin from the edited crate alone. The behavior may be owned elsewhere.

## When the user provides nuance

Treat the message as a semantic delta.

Write down privately:

- which cases were previously conflated;
- which contextual axis now matters;
- what should change;
- what must remain unchanged;
- what observable consequence distinguishes the cases;
- whether current code violates the clarified contract.

Then update the canonical feature corpus during the same work.

### Classification

Use:

- `built` when implementation and mapped evidence already satisfy the clarified behavior;
- `specified` when implementation, evidence, fixture, or platform work remains;
- `known-violation` when current behavior contradicts the intended current contract.

Open or update a GitHub issue for `specified` and `known-violation`. The issue owns work; the feature owns meaning.

## Write the behavior before adapting the implementation

For a nuanced change:

1. locate the owning `Feature` and `Rule`;
2. correct wrong prose in place;
3. add the smallest contrastive scenarios;
4. assign stable IDs and truthful status;
5. record the tempting wrong behavior in the falsifier;
6. choose the evidence layer.

Do not write a feature file per changed crate. Do not duplicate the issue body.

## Choose proof deliberately

Use [`test-placement.md`](test-placement.md).

Ask:

- Is this a local invariant, broad state-space property, persistence claim, facade promise, platform projection, or live compatibility claim?
- What is the smallest test that structurally excludes the bug?
- Is a public facade capstone also needed?
- Does the scenario require restart, fault ordering, or an independent witness?

Write the primary proof where the contract lives.

## Establish red evidence

Before fixing the code:

- reproduce the old/incorrect behavior;
- add or update the proof so it fails for the semantic distinction;
- confirm the failure is not merely setup breakage;
- save a minimal diagnostic when the failure is asynchronous.

For existing passing behavior whose proof is new, perform the mechanism-disable falsifier: temporarily remove or invert the relevant mechanism and prove the test turns red.

Do not accept a test that passes before and after removing the behavior it claims to prove.

## Inspect fixtures for answer injection

For each `Given` or setup helper, ask:

- Is this a real input or the derived answer?
- Am I preloading the route, evidence, identity, capability, or state the product is meant to discover?
- Am I observing public behavior, or reading the same private state I inserted?
- Can the system self-certify an external effect that should have an independent witness?

This check is mandatory for discovery, routing, coverage/evidence, restart, and delivery scenarios.

## Implement the structural mechanism

Prefer changes that make the wrong state unreachable or explicit:

- typed distinctions instead of ambiguous booleans;
- scoped evidence keys instead of aggregate claims;
- frozen identity at acceptance instead of late default lookup;
- durable state transitions instead of inferred success;
- explicit shortfall instead of silent truncation;
- dependency tracking instead of broad rerooting;
- one owner for protocol composition/validation.

The test should validate the behavior, but the code structure should carry the guarantee.

## Run the proof stack

Run, in order:

1. focused failing/passing test;
2. owning crate unit/property/integration tests;
3. `nmp` facade integration tests when the behavior crosses the facade;
4. acceptance capstones mapped by the feature;
5. restart/fault tests where claimed;
6. parity/native tests where the boundary participates;
7. formatting, lint, dependency/ownership, and workspace gates.

Do not substitute a broad workspace pass for checking that the intended falsifier turned red.

## Promote status and update traceability

After all required evidence passes:

- set `nmp:status=built`;
- add exact evidence locators;
- add or refine the falsifier;
- remove obsolete `gap` and issue metadata only when no active violation/work remains;
- update the bug-class ledger only if a class-level closure claim changes;
- correct architecture docs if the guarantee or mechanism changed.

Close the GitHub issue only when its work is complete. Do not leave a stale `specified` scenario or a `built` claim with missing evidence.

## PR/change summary

State:

- behavioral IDs changed or added;
- semantic distinction captured;
- structural mechanism that enforces it;
- primary evidence and falsifier;
- restart/platform/live coverage where relevant;
- any remaining `specified` or `known-violation` scenarios with issue links.

Avoid vague summaries such as “added BDD coverage.”

## When refactoring without intended behavior change

1. identify affected feature IDs and evidence;
2. run their proofs before the refactor;
3. keep behavior prose unchanged unless you discover it was inaccurate;
4. prefer tests through stable public contracts so internal movement does not require scenario rewrites;
5. run mechanism-disable or equivalent checks when replacing a load-bearing path;
6. update evidence locators when tests move.

A refactor that forces acceptance scenarios to mention new private types is evidence that the test boundary is wrong.

## When a test and feature disagree

Do not automatically trust either.

Determine whether:

- the feature captured an explicit user decision;
- the test encoded old behavior;
- the feature is stale or overgeneralized;
- the fixture stages a different condition from the prose;
- the evidence proves only a narrower claim.

Correct the authoritative behavior in place, then align implementation and proof. Never weaken the feature merely to make the current test green.

## Prohibited shortcuts

Do not:

- leave user nuance only in chat or a PR;
- add `@wip` without identifying the exact gap;
- mark behavior built without evidence;
- use Cucumber as the default proof for every invariant;
- add a feature scenario solely because a regression test exists;
- inspect internal tables for a public acceptance claim;
- inject a discovered route/evidence result in setup;
- increase sleeps/retries to conceal nondeterminism;
- duplicate one acceptance path across Rust, shell, and Cucumber without a distinct proof purpose;
- create another planning document instead of using the owning GitHub issue.

## Completion check

Before ending the task, use [`review-checklist.md`](review-checklist.md). A behavior change is incomplete if its English-language meaning or proof mapping is missing even when all code tests pass.
