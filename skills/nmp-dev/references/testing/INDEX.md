# NMP testing

NMP uses two connected but distinct systems:

- **Feature files preserve behavioral meaning.** They are the durable, English-language memory of how NMP should behave, including contextual distinctions, edge cases, counterexamples, and explicit corrections from the user.
- **Executable tests provide evidence.** They prove those behavioral claims at the narrowest stable contract that owns the guarantee.

Do not collapse these into one system. Not every meaningful scenario belongs in Cucumber, and not every executable invariant needs a feature scenario.

## Non-negotiable principles

1. **A user correction is a specification event.** When the user explains that two apparently similar cases must behave differently, preserve that distinction in the canonical feature corpus.
2. **Feature placement follows behavioral domains.** Routing behavior belongs with routing behavior even when five Rust crates participate.
3. **Test placement follows contract ownership.** A router invariant belongs in `nmp-router`; a public cross-component promise belongs in `crates/nmp/tests`; platform projections belong in parity and native suites.
4. **Meaning and proof are traceable, not duplicated.** A scenario names the behavior. Its metadata points to the executable evidence. The test does not need to repeat the scenario prose verbatim.
5. **Acceptance tests use the supported facade.** Controlled setup may use test-only seams. Scenario actions and observations must not bypass the contract they claim to prove.
6. **Distributed claims require appropriate falsifiers.** Restart, failure ordering, independent witnesses, and deterministic fault schedules matter more than another happy-path example.
7. **A green test is not evidence if the fixture injected the answer.** Stage causes, not derived outcomes.

## Mandatory response to behavioral nuance

Whenever the user says that behavior is wrong, incomplete, context-dependent, or easily misinterpreted:

1. Identify the distinction the user introduced.
2. Find the canonical feature and `Rule` that own it.
3. Update an existing scenario or add contrastive scenarios that separate the cases.
4. Mark the scenario status truthfully.
5. Add or update executable evidence at the correct test layer.
6. Prove the evidence would fail if the named mechanism or distinction were removed.
7. Link active implementation work to a GitHub issue without turning the feature corpus into a task list.

Do this as part of the same work. Do not leave the nuance only in chat, a PR description, or a test name.

## Where to start

| Need | Read |
|---|---|
| Capture a correction or nuanced behavior | [`behavior-specification.md`](behavior-specification.md) |
| Record status, evidence, and falsifiers | [`evidence-and-traceability.md`](evidence-and-traceability.md) |
| Decide which crate or suite owns the proof | [`test-placement.md`](test-placement.md) |
| Test routing, acquisition, source, account, or evidence context | [`routing-and-context.md`](routing-and-context.md) |
| Test restart, timing, partial failure, ACK ambiguity, or boundedness | [`distributed-systems.md`](distributed-systems.md) |
| Follow the expected workflow while implementing | [`agent-development-workflow.md`](agent-development-workflow.md) |
| Review a test or behavior change | [`review-checklist.md`](review-checklist.md) |

## Test-layer decision table

| Claim | Primary proof |
|---|---|
| Pure value, parser, codec, or local state transition | Unit or table-driven test in the owning crate |
| Invariant over a large input space | Property, model, or differential test in the owning crate |
| Persistence or reconstruction guarantee | Store/owner integration test with close and reopen |
| Behavior spanning mechanisms but owned by the Rust product facade | `crates/nmp/tests/` facade integration test |
| High-value cross-layer public promise worth reading as a capstone | `@acceptance` feature scenario driven through `nmp` |
| Cross-language semantic or byte equivalence | `nmp-parity` plus native SDK tests |
| Public-network/provider compatibility | Opt-in live probe |
| Timing, retry, reordering, partial failure, or crash interleaving | Deterministic fault/schedule or state-machine test |

A behavior can require several proof layers. Prefer one structural proof at the owning mechanism and one public capstone when the product-level consequence is important. Do not reproduce the same integration scenario in every crate it touches.

## Canonical responsibilities

| Surface | Owns |
|---|---|
| Feature files | Behavioral meaning and discriminating examples |
| Executable tests | Evidence that current code satisfies the behavior |
| GitHub issues | Temporary implementation, repair, or evidence work |
| Architecture/design docs | Why the system is structured as it is |
| Bug-class ledger | Class-level closure claim, structural exclusion, and proof summary |
| PR description | What changed in this change set |

Do not make any one surface impersonate the others.

## Feature-corpus organization

Organize features by stable behavioral domain, not by Rust crate:

```text
features/
  acquisition/
  routing/
  identity/
  writes/
  evidence/
  limits/
  protocol-composition/
  lifecycle/
  reset/
  must-never/
```

Crates may split or merge. The behavioral distinctions should survive those refactors.

## Definition of done for a behavioral change

A behavioral change is not complete until:

- the canonical English-language behavior is accurate;
- its status is truthful;
- executable evidence exists at the correct layer or an issue records the missing work;
- the old or incorrect behavior is falsified;
- fixtures do not preload the claimed result;
- relevant restart, context, platform, and failure dimensions have been considered;
- all evidence locators still resolve and pass.
