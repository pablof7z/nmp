---
name: nmp-dev
description: Develop, fix, refactor, review, or test the NMP repository itself across Rust crates, the supported facade, FFI, Swift and Kotlin projections, feature/BDD suites, persistence and fault tests, documentation, and repository tooling. Use when work changes this repository or evaluates its internal design and evidence. Do not use for an application that merely consumes NMP; use the separate nmp skill.
---

# NMP repository development

Treat the root `AGENTS.md` as the canonical contributor guide. Use this skill to route internal work and load detailed testing guidance; do not let it override current repository docs, GitHub issues, or source.

## Establish the work boundary

1. Find the repository root and read `AGENTS.md` completely.
2. Follow its cold-start order before proposing architecture or changing behavior.
3. Find the owning GitHub issue. If no issue captures the consequence and honest rationale, file it before editing.
4. Work on an issue-linked branch in an isolated worktree. Inspect its status first and preserve unrelated changes.
5. Read the current owning design document, bug-class ledger row, known gap, feature rule, and tests that govern the change. Treat plans as temporary work artifacts, not architecture authority.

For read-only explanation or review, inspect the same authorities but do not create repository or GitHub state unless the user also asked for a change.

## Preserve NMP's architecture

- Keep the app-facing model to a live query and a write intent. Diagnostics are proof over those nouns, not a third command surface.
- Prefer a type or API shape that excludes the bad path plus a falsifier that proves the exclusion. Prose and review memory are not structural mechanisms.
- Apply the standing conventions before surface work: remove replaced spellings in the same change, keep Bech32 at human boundaries, do not invent protocol categories or repository jargon, and ship requested behavior active on the normal runtime path without an unrequested runtime feature gate. Read [`no-hidden-runtime-feature-flags.md`](../../docs/internals/conventions/no-hidden-runtime-feature-flags.md) before proposing a flag, environment switch, config boolean, rollout toggle, experimental mode, or opt-in for requested behavior.
- Run the Noun, Reachability, Bool-Lifecycle, and Destructive-API gates by eye. Run cross-SDK parity and falsifier-honesty mechanically when the affected surface requires them.
- Fix governed behavior end to end across the Rust facade, persistence or diagnostics, FFI, Swift, Kotlin, docs, and falsifiers. Do not preserve a compatibility path.
- Keep `README.md`, `docs/known-gaps.md`, and `docs/bug-class-ledger.md` truthful. Change a gap or ledger status only when the claimed implementation and proof actually changed.

## Route testing and behavioral work

Start with [NMP testing](references/testing/INDEX.md), then load only the references needed for the claim:

- Preserve user corrections, contextual distinctions, and negative guarantees in the feature corpus: [Behavioral specification](references/testing/behavior-specification.md).
- Map truthful status, exact evidence, and a mechanism-disable falsifier: [Evidence and traceability](references/testing/evidence-and-traceability.md).
- Put executable proof at the narrowest stable contract owner and add a facade capstone only when the public consequence needs it: [Test placement](references/testing/test-placement.md).
- Make identity, source, relay role, access context, request scope, lifecycle, and write phase explicit for acquisition work: [Routing and context testing](references/testing/routing-and-context.md).
- Use restart, controlled schedules, fault points, ambiguity, independent witnesses, and reproducible artifacts for distributed claims: [Distributed-systems testing](references/testing/distributed-systems.md).
- Follow the red-to-green implementation sequence and status-promotion discipline: [Agent development workflow](references/testing/agent-development-workflow.md).
- Apply the final behavioral, fixture, evidence, distributed, and test-architecture checks: [Testing review checklist](references/testing/review-checklist.md).

The testing references describe how to develop and review the corpus. Before applying their examples or metadata, inspect `docs/bdd/000-bdd-approach.md`, the current `features/` tree, and `crates/nmp-bdd`. If current practice differs, keep status honest and use the owning issue to reconcile it; never infer that a scenario or platform guarantee is already built.

## Execute and prove the change

1. Identify the narrowest owner of the guarantee before choosing files or test targets.
2. Establish red evidence for a defect or new distinction. For already-correct behavior with missing proof, temporarily disable or invert the named mechanism and confirm the proof fails for the intended reason.
3. Inspect fixtures for answer injection. Stage causes and observe supported behavior or an independent external witness.
4. Implement the structural mechanism. Update every governed caller and projection in the same change.
5. Run the focused falsifier, then touched-crate tests, facade/BDD capstones, restart or fault tests, parity/native suites, and repository gates in proportion to the claim.
6. Verify the running consumer or system path when the change claims runtime behavior. Compilation is not execution evidence.

Do not substitute a workspace-wide green run for proving that the intended falsifier turns red and then green.

## Finish and hand off

- Review the diff against all six architecture gates and the issue scope.
- Report the exact commands run and distinguish deterministic proof from live or platform evidence not run.
- Close the issue only when its work is complete. Otherwise leave a GitHub issue or PR handoff note with the exact branch, worktree, blocker, and next step.
- If a worktree, branch, blocker, or unmerged PR remains, make that external handoff before ending the session.
