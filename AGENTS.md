# AGENTS.md

Canonical contributor guide for the NMP repo. Every rule here applies to agents and humans alike. Keep it concise and plain: durable understanding lives in `docs/`, and temporary work lives in GitHub Issues. Execution plans are temporary work artifacts, not architecture authority; move lasting decisions into the document that owns the subject and let git preserve the plan's history.

## Cold-start reading order

1. `README.md` — what NMP is (two nouns: a live query, a write intent) and the honest current status.
2. `docs/VISION.md` — the north star, the milestone plan (M0–M6), the two thesis-gates, and the numbered principles (P1…) work is measured against.
3. `docs/builder/28-patterns.md` — the numbered bug classes the design structurally rules out, and the mechanism that rules each out. Correctness lives in the shape of the API, not in a police force patrolling it; the usual mechanism is *absence* — the unsupported thing has no API, so no caller can express it.
4. `docs/known-gaps.md` — the truth-anchor companion: everything built-but-incomplete or deliberately deferred, so nothing hides.
5. `docs/internals/architecture-boundaries.md` — where a decision ends, where a commit begins, and what may happen before it. What "functional" and "reactive" mean *here*, the transaction/effect rules, and the ownership rules — plus the current honest exceptions to each.
6. `docs/internals/crate-architecture.md` — the target crate set: what each crate owns, which *seams* are settled (engine/runtime, capability eviction, the store transaction), which questions are still open (including the internal decomposition of the deterministic engine), the capability rule (`nmp` knows no event-kind capability) with its measure, and the rule that a crate is first a unit of responsibility and authority. A distinct dependency list is not required. Issues encode older decisions; this is the destination they are checked against.
7. **GitHub Issues** — the single tactical tracker: what is being worked on, what is queued, and the *why* behind each.

## Internal-development skill

For work that changes NMP itself, use `skills/nmp-dev/SKILL.md`. It routes internal implementation, review, and testing work; for behavioral changes, test changes, or user corrections about how NMP should behave, start with `skills/nmp-dev/references/testing/INDEX.md`. `skills/nmp/SKILL.md` is the separate consumer-facing skill for applications that build with NMP; it is not authority for NMP internals.

## Working discipline

- **NMP has no external consumers.** Every caller of every surface is inside this workspace or a sibling repository that moves with it.
- **Branches + PRs, never push work straight to `master` from a shared build.** Agents work in isolated git worktrees; a cohesive feature is one PR in one shared worktree.
- **The commit message carries the why.** State the consequence — what breaks, what a user can't do, what invariant is unproven — not just the mechanical change. An honest small reason ("this is a plain bug", "mechanical cleanup", "this unblocks a clean clone") beats an invented grand one; a fabricated justification is worse than no justification.
- **Truth and honesty are the anchors.** The README is the current honest picture, not a pitch, and not a changelog. `docs/known-gaps.md` must list what doesn't work. Compiles ≠ works — verify the running result.
- **Fix end-to-end.** No temporary hacks, no compat aliases, no narrating a defect instead of fixing it. If a change is right, make it and update every caller in the same PR.
- **Test scope:** run the tests for the crates you touched (`cargo test -p <crate>`); a workspace run is the merge-time gate, not the per-change loop.
- **One PR, one owner or one invariant.** A PR establishes one coherent owner or closes one invariant, and carries its implementation, focused tests, the integration tests it affects, and doc corrections together. Do not split preparatory wrappers, aliases, test rewrites, and cleanup into separate PRs just to keep diffs small — that scatters one reviewable change across several unreviewable ones.
- **Merge overlapping work sequentially.** When two PRs touch the same area, land them one at a time: rebase the second onto exact `master`, rerun its evidence, and inspect the conflict resolution by hand before merging — do not trust an automatic merge to preserve either PR's invariants.
- **Prove a new invariant by breaking it.** Before merging a PR that closes an invariant, deliberately violate it and confirm the focused test fails for the intended reason. A passing count is not sufficient proof where the claim is an exact relationship, not a quantity.
- **Unexplained flakes and semantic ordering changes are merge blockers**, not follow-up issues — land the fix or the explanation first.
- **Pause for an integration checkpoint after every two or three owner extractions** in a decomposition sequence, rather than stacking many extractions unverified.
- **CI returns once local commands are deterministic** and any known flakes are fixed, or excluded with an issue that owns them. When it returns it starts small: each gate protects one named failure mode — isolated crate/build shapes, capability-composition consumer builds, focused owner and headless falsifiers, public-API system scenarios, and, as the merge gate, the full workspace test run. A gate earns its place only if reverting the mechanism it protects makes the gate fail; a source grep, a line- or count-threshold, a tombstone check, or a broad "green" command never shown to catch the claimed defect does not qualify.
- **Hand off out loud.** Before ending a session or handing off in-progress work — a git worktree left behind, a blocker punted to someone else, a PR not yet merged — leave a clear handoff note on the owning GitHub issue/PR itself: the exact branch/worktree name, the current blocker, and the next step. This is the required, universally-reachable mechanism.
