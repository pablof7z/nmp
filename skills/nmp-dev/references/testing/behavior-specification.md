# Behavioral specification

Feature files are NMP's durable behavioral memory. Their purpose is to preserve meaning that would otherwise be lost across agent sessions, implementation rewrites, issue closure, and test refactors.

They are not merely Cucumber input.

## What must enter the feature corpus

Capture a behavior when the user provides any of the following:

- a correction: “that is not how it should work”;
- a contextual distinction: “in this case, but not in that case”;
- a non-obvious routing or identity rule;
- a negative guarantee: “this must never cause…”;
- a durability claim: “this must still be true after restart”;
- a truthfulness requirement: “do not call this complete merely because…”;
- a counterexample that invalidates the current abstraction;
- an explicit preference between plausible product behaviors;
- a clarification that a familiar word such as “sync,” “coverage,” “accepted,” or “account” is too broad.

Do not wait until implementation begins. The correction itself is the reason to update the behavioral memory.

## Extract the semantic delta

Before writing a scenario, state the distinction in one sentence:

> Previously these cases were treated as equivalent; the user has now established that **X** changes the result while **Y** must not.

Then identify:

- the contextual axis that differs;
- the action whose meaning is affected;
- the observable consequence;
- the incorrect shortcut or conflation the example prevents.

This produces better scenarios than paraphrasing the user's message line by line.

## Prefer contrastive examples

A single positive example often fails to preserve the nuance. When the behavior depends on context, write the smallest pair or set of scenarios that separates the cases.

```gherkin
Feature: Active-account routing

  Rule: Only demand that depends on the active account reroutes

    # nmp:id=ROUTING-ACCOUNT-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::reactive_demand_reroots_on_active_account_change
    # nmp:falsifier=Make all demand reroot on active-account change; this scenario must fail.
    Scenario: Reactive demand follows the new active account
      Given a query whose author set depends on the active account
      And Alice is the active account
      When the active account changes to Bob
      Then the query uses Bob's derived sources

    # nmp:id=ROUTING-ACCOUNT-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-router::literal_demand_remains_pinned
    # nmp:falsifier=Reroot literal demand on active-account change; this scenario must fail.
    Scenario: Literal demand remains pinned
      Given a query with Alice's pubkey as a literal author
      And Alice is the active account
      When the active account changes to Bob
      Then the query still uses Alice's sources
```

The second scenario is not redundant. It preserves the boundary of the rule.

## Choose the right Gherkin construct

### `Feature`

Use a feature for a stable behavioral capability such as routing, write durability, acquisition evidence, or trust-domain reset.

### `Rule`

Use a rule for the principle that explains several examples. Put the “why these cases differ” statement here.

### `Scenario`

Use a scenario for one discriminating example with one promise in its title.

### `Scenario Outline`

Use an outline when a small, finite matrix is itself meaningful to readers. Do not use it to enumerate a state space better covered by property tests.

### Prose and tables

Short explanatory prose and decision tables are allowed inside feature files. Do not force every invariant into ceremonial Given/When/Then syntax. The requirement is precise, durable behavioral meaning and valid Gherkin where scenarios are used.

## Write in behavioral vocabulary

Use words an application or protocol-aware developer can reason about:

- app, account, identity, query, source, relay, request, write, receipt;
- cached rows, acquisition evidence, EOSE, unavailable source, shortfall;
- accepted, awaiting signer, signed, attempted, acknowledged, rejected;
- restart, reconstruct, reconnect, reset.

Avoid implementation vocabulary unless the implementation boundary is itself contractual:

- private Rust type names;
- table names;
- reducer variants;
- internal channels;
- exact helper calls;
- crate ownership claims disguised as product behavior.

Bad:

```gherkin
Then the resolver's LiveDirectory contains Alice's relay
```

Better:

```gherkin
Then Alice's content is requested from the relay named by her relay-list event
```

## Stage causes, not answers

A scenario that claims discovery must stage the discoverable fact, not directly inject the discovered result.

Bad self-bootstrap fixture:

- insert Alice's write relay directly into the engine directory;
- assert that Alice's relay is later contacted.

Truthful fixture:

- configure only the indexer initially;
- seed Alice's relay-list event at the indexer;
- observe the indexer request;
- observe the discovered relay being contacted afterward;
- observe the final result through the public facade.

Ask of every `Given`:

> Is this an input the real product receives, or is it the internal conclusion the product is supposed to derive?

## Scenario metadata

Every scenario uses a structured comment block immediately above it.

```gherkin
# nmp:id=ROUTING-DISCOVERY-003
# nmp:status=built
# nmp:evidence=rust:nmp::self_bootstrap_discovers_write_relay
# nmp:evidence=rust:nmp-nip65::relay_list_decodes_write_relay
# nmp:falsifier=Disable relay-list ingestion; the content relay must never be contacted.
@acceptance
Scenario: An author relay is discovered without an app-supplied route
  ...
```

### Required fields

- `nmp:id`: stable, unique behavioral identifier.
- `nmp:status`: exactly one of `built`, `specified`, or `known-violation`.

For `built`:

- one or more `nmp:evidence` lines;
- one `nmp:falsifier` line.

For `specified`:

- `nmp:gap=implementation`, `evidence`, `fixture`, or `platform`;
- `nmp:issue=#N`.

For `known-violation`:

- `nmp:issue=#N`;
- preferably evidence pointing to the reproducer.

### Status meaning

| Status | Meaning |
|---|---|
| `built` | Current behavior is implemented and mapped evidence passes |
| `specified` | Behavior is agreed, but implementation or sufficient evidence has not been promoted |
| `known-violation` | The scenario states the intended current contract, and current code is known to contradict it |

Do not use an ambiguous `@wip` state. The exact gap must be stated.

`@acceptance` means the scenario itself is executed as a facade-level acceptance capstone. It is not a status. An `@acceptance` scenario must be `built`.

## Stable IDs

Use IDs of the form:

```text
<DOMAIN>-<CONTEXT>-<NNN>
```

Examples:

- `ROUTING-ACCOUNT-001`
- `EVIDENCE-EOSE-004`
- `WRITES-RESTART-002`
- `MUSTNEVER-AUTH-003`

Keep an ID when refining the same promise. Split into new IDs when one scenario concealed several independently meaningful promises. Delete obsolete behavior rather than preserving historical contradictions. Never reuse a deleted ID for different meaning.

## Updating existing behavior

When a user correction conflicts with an existing scenario:

1. Decide whether the old scenario was wrong, incomplete, or described a different context.
2. Correct it in place if it was wrong.
3. Split it if it conflated distinct contexts.
4. Add a contrasting scenario if the boundary is the new information.
5. Update status and evidence.
6. Open or update the implementation issue when current code no longer satisfies the corrected contract.

Do not add an appendix that leaves the old claim intact. Do not create a second feature that quietly contradicts the first.

## What does not need a feature scenario

Do not add a scenario solely because a test exists. A local implementation detail usually needs only executable evidence.

Feature scenarios are warranted when the behavior is:

- product-meaningful;
- nuanced or easy to flatten;
- externally consequential;
- a user-provided correction;
- a durable boundary between valid and invalid interpretations;
- useful to an agent deciding what code should do.

A parser edge case, exhaustive codec matrix, private enum transition, or arbitrary regression may remain only in its owning test suite unless it expresses one of those distinctions.

## Quality test for a scenario

A strong scenario answers all four questions:

1. What context matters?
2. What action occurs?
3. What observable result follows?
4. What tempting incorrect interpretation does this example rule out?

If the fourth answer is unclear, the scenario may be decorative rather than useful behavioral memory.
