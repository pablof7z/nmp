---
name: nmp-dev
description: Develop, review, test, or refactor NMP itself. Always use this skill when working in the NMP repository. For apps consuming NMP, use the separate nmp skill.
---

# NMP development

`AGENTS.md` is canonical. Repository docs, issues, and source outrank this
router.

## Start

1. Read `AGENTS.md` and its cold-start sources.
2. Find or file the issue before editing.
3. Use an issue-linked branch in an isolated worktree.
4. Read the owning design, gap, feature rule, and tests.

For behavior, tests, or user corrections, start with [NMP
testing](references/testing/INDEX.md).

## Communicate plainly

- Lead with the concrete behavior and its consequence. Add exact symbols, test
  names, and repository terms only after the explanation.
- Use the words the user used when they are accurate. Define a necessary NMP or
  Nostr term the first time it appears.
- Do not compress several rules into one list of abstract nouns. Use separate
  sentences or questions so each rule can be understood and challenged.
- Do not make a narrow observation sound reusable by giving it a broad name.
  State where it applies and where it does not.

## Guardrails

- Keep the public model to a live query and a write intent.
- Add read capabilities through `LiveQuery` and write capabilities through
  `WriteIntent`. Helpers may return typed values or help build those requests.
- When one live request produces another, NMP must remember that relationship
  and update the produced request when its input changes. Keep the identity,
  source, access rules, cache rules, cached results, and freshness of each
  request separate. Do not reuse any of them for a different request.
- Do not make apps manage internal rows or combine several observations to get
  one capability. Do not create a separate observation or receipt lifecycle
  for one capability.
- Make invalid use impossible through the API. Add a test that fails if that
  protection is removed.
- Apply every standing convention in `AGENTS.md`, including [no hidden runtime
  flags](../../docs/internals/conventions/no-hidden-runtime-feature-flags.md).
- Apply `AGENTS.md`'s standing conventions in proportion to the diff. There is no PR-review checklist to run by eye — the repo deleted that ceremony; the conventions are design-time rules, and a violation is a defect to fix, not a box to check.
- When behavior shared across layers changes, update every affected layer in
  the same change: Rust, storage, diagnostics, FFI, Swift, Kotlin, docs, tests,
  validation scripts, and evidence metadata. Do not keep the old path for
  compatibility.
- Keep `README.md`, `docs/known-gaps.md`, and the bug-class ledger truthful.
- A crate is a unit of responsibility and authority. Cargo is one way of
  making that boundary structural. Do not refuse a package because both
  sides still share lower-level dependencies. See
  `docs/internals/crate-architecture.md`.

## Execute

1. State what behavior is wrong and identify the smallest part of NMP
   responsible for it.
2. Before changing code, show a test that fails. If the behavior already works
   and only proof is missing, temporarily remove its protection and confirm the
   new test catches the break.
3. In test setup, provide the inputs that should produce the result. Do not
   insert the result itself.
4. Fix the shared source of the problem. Update every affected caller and
   platform layer in the same change.
5. Run the test that demonstrates the bug, tests for changed crates, and any
   public-API, restart, fault, parity, native, or repository checks the claim
   depends on.
6. Verify the running path for runtime claims. Compilation is not execution.

## Finish

- Review the diff against the issue and `AGENTS.md`'s standing conventions.
- Report the commands run, what they proved, and any live or platform checks
  not run.
- Close completed issues. Otherwise post branch, worktree, blocker, and next
  step on the issue or PR.
