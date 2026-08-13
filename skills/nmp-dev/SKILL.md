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

## Guardrails

- Keep the public model to a live query and a write intent.
- Extend those nouns for new capabilities. Helpers may add typed values and
  composable bindings, but must preserve reactive identity and each demand's
  source, access, cache, and freshness context. No capability-specific
  lifecycles, public row-projection boilerplate, or app-managed waterfalls.
- Exclude bad paths by type/API shape; prove exclusion with a falsifier.
- Apply every standing convention in `AGENTS.md`, including [no hidden runtime
  flags](../../docs/internals/conventions/no-hidden-runtime-feature-flags.md).
- Run all six architecture gates in proportion to the diff.
- Change Rust, persistence, diagnostics, FFI, Swift, Kotlin, docs, and proofs
  together when their contract changes. Add no compatibility path.
- Keep `README.md`, `docs/known-gaps.md`, and the bug-class ledger truthful.

## Execute

1. Name the narrowest contract owner and the failing claim.
2. Prove red before implementation, or disable the claimed mechanism to prove
   an already-correct path would fail without it.
3. Stage causes, not derived answers.
4. Implement the structural fix; update every governed caller and projection.
5. Run the focused falsifier, touched-crate tests, and relevant facade, restart,
   fault, parity, native, and repository checks.
6. Verify the running path for runtime claims. Compilation is not execution.

## Finish

- Review the diff against the issue and six gates.
- Report exact proof and unrun live/platform checks.
- Close completed issues. Otherwise post branch, worktree, blocker, and next
  step on the issue or PR.
