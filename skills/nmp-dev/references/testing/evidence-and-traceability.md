# Evidence and traceability

Feature files state what NMP means. Tests prove that current code honors that meaning. Traceability connects them without forcing both into the same file, runner, or abstraction level.

## The evidence contract

Every `built` scenario must identify executable evidence that:

- exercises the relevant mechanism;
- would fail for the defect the scenario is meant to exclude;
- asserts at the appropriate contract boundary;
- runs in a required test lane unless explicitly classified as a live probe;
- is specific enough to distinguish the scenario from its contrasting cases.

Passing nearby tests is not evidence by association.

## Evidence locator format

Use one `# nmp:evidence=` line per proof.

```text
# nmp:evidence=<kind>:<owner>::<test-or-check>
```

Recommended kinds:

- `rust` — Rust unit, property, integration, or acceptance test;
- `swift` — Swift projection or parity test;
- `kotlin` — Kotlin projection or parity test;
- `parity` — shared cross-platform fixture/check;
- `script` — repository check with a stable command or script path;
- `live` — opt-in real-provider or public-network probe.

Examples:

```text
# nmp:evidence=rust:nmp-router::literal_demand_remains_pinned
# nmp:evidence=rust:nmp::accepted_write_survives_reopen
# nmp:evidence=swift:NmpTests::receipt_reconstructs_after_restart
# nmp:evidence=parity:nmp-parity::nip22_comment_bytes
# nmp:evidence=script:scripts/check-schema-ownership.sh
```

Prefer stable test names over line numbers. Evidence locators must be greppable and kept current when tests move or are renamed.

## One scenario, several proofs

Some claims require multiple layers:

- a property test proves the mechanism over a broad state space;
- a facade integration test proves the public consequence;
- a restart test proves durability;
- native tests prove projection parity.

List all load-bearing evidence, not every incidental test.

Example:

```text
# nmp:evidence=rust:nmp-router::reroot_is_dependency_scoped
# nmp:evidence=rust:nmp::active_account_change_preserves_literal_subscription
# nmp:evidence=swift:NmpTests::literal_subscription_survives_account_change
```

## One proof, several scenarios

A property or model test may prove several examples. Reusing the same locator is valid when the test genuinely distinguishes all mapped scenarios. The test name and failure output should make that scope intelligible.

Do not duplicate tests merely to create one-to-one bookkeeping.

## Falsifier requirement

Every `built` scenario must state the product mutation or defect that should make its evidence fail:

```text
# nmp:falsifier=Treat one relay's EOSE as global completion; this scenario must fail.
```

When introducing or materially changing evidence:

1. temporarily disable, bypass, or invert the named mechanism;
2. run the mapped proof;
3. confirm it fails for the intended reason;
4. restore the implementation;
5. run the proof again.

This is a review discipline. It does not require a permanent mutation-testing framework.

A test that remains green when its named mechanism is removed is not evidence for that behavior.

## Prove the semantic delta

For a correction, first reproduce the old or incorrect behavior. The proof should fail before the fix and pass after it.

Prefer a test whose failure demonstrates the newly introduced distinction:

- reactive demand reroutes, literal demand does not;
- one source reached EOSE, another remains unavailable;
- accepted work survives restart, an unaccepted draft does not;
- explicit identity override remains pinned even after the default account changes.

A broad “returns expected rows” assertion is usually too weak.

## Independent observables

Prefer oracles outside the mechanism making the claim.

Good independent witnesses:

- a scripted relay records whether it was contacted;
- a process supervisor records exact lifecycle effects;
- a reopened facade reports reconstructed state;
- a consumer receives a typed frame or receipt;
- a platform projection produces the same canonical bytes.

Weak self-attestation:

- the router says it selected only allowed relays;
- the store row says a write was durable without reopening it;
- diagnostics claim no contact while no relay witness exists;
- an internal directory contains the route the fixture inserted directly.

Internal diagnostics are useful evidence, but do not let the system certify its own external effects when an independent witness is practical.

## Status promotion

### Promote to `built` only when

- implementation exists;
- mapped evidence passes;
- the evidence fails under the stated falsifier;
- required restart/platform/failure dimensions are covered;
- all evidence locators resolve;
- any `@acceptance` scenario executes through the supported facade.

### Use `specified` when

The behavior is agreed but one of these gaps remains:

```text
# nmp:status=specified
# nmp:gap=implementation
# nmp:issue=#123
```

Allowed gaps:

- `implementation` — the product behavior is not built;
- `evidence` — behavior may exist, but adequate proof is missing;
- `fixture` — the required deterministic environment cannot yet stage or observe the behavior truthfully;
- `platform` — a required platform projection is missing.

Do not claim “built” merely because code appears to implement the behavior.

### Use `known-violation` when

The scenario describes the intended current contract and a current defect contradicts it:

```text
# nmp:status=known-violation
# nmp:issue=#456
# nmp:evidence=rust:nmp::repro_duplicate_ack_after_restart
```

The reproducer may intentionally fail or be encoded as a test of current known behavior in a quarantined lane. The issue owns the repair work; the feature owns the intended behavior.

## Avoid duplicate truth

Keep responsibilities separate:

- The feature scenario explains the behavior.
- Evidence metadata points to proof.
- The test contains executable assertions.
- The issue contains implementation tasks and current blockers.
- The bug-class ledger summarizes class-level closure and structural exclusion.
- Architecture docs explain the mechanism and design rationale.

Do not copy the full issue plan into the feature file. Do not copy all scenario prose into the test comments. Do not use feature tags as a project board.

## Acceptance-specific rules

An `@acceptance` scenario is evidence only when:

- it drives the canonical `nmp` facade;
- setup controls the environment without performing the behavior under test;
- actions use supported operations;
- `Then` steps observe facade output, typed failure, reconstruction, or an external witness;
- it uses the exact product build under test;
- teardown proves no process, relay, thread, port, file, or environment state leaks.

Direct dependencies on mechanism crates require a narrow, documented fixture reason. They must not be used to drive or inspect the claimed public behavior.

## Traceability checks to automate

Add or preserve a repository check that verifies:

- every scenario has one unique `nmp:id`;
- every scenario has one valid status;
- every `built` scenario has evidence and a falsifier;
- every `specified` scenario has a gap and issue;
- every `known-violation` scenario has an issue;
- every `@acceptance` scenario is `built`;
- evidence locators resolve to known files/tests where mechanically possible;
- no ID is duplicated;
- all feature files parse;
- no ambiguous `@wip` or `@designed` status remains.

The linter checks bookkeeping. It does not establish semantic truth; review and falsification do that.

## Review question

For each evidence mapping, ask:

> Can the named defect still exist while this evidence remains green?

If yes, strengthen or replace the evidence before promoting the scenario.
