---
title: Protected-path changes need a different agent's signoff and a mosaico note
category: conventions
slug: protected-path-signoff
status: policy
date: 2026-07-31
owns:
  - the rule binding any change to a protected governance path or prefix
  - what "a completely different agent" means and what the signoff must cover
  - why this control is procedural rather than mechanical, and when it is deleted
  - why it is not a general review gate and does not reopen #592
related:
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/design/architecture-review-gates.md
issues:
  - https://github.com/pablof7z/nmp/issues/608
  - https://github.com/pablof7z/nmp/issues/1183
---

# Protected-path changes need a different agent's signoff and a mosaico note

Pablo (repository owner, 2026-07-31):

> let's put that, as a rule, agents need to get the signoff from a completely
> different agent and they should leave a note for me in the mosaico channel
> tagging me about it.

---

## 1. What a protected path is — BUILT

`scripts/check-surface-migration-authorization.py` owns the list, in
`PRODUCTION_POLICY`: `protected_paths` (exact files, matched as namespaces) and
`protected_prefixes`.

**Read it. Do not carry a summary of it in your head, and do not copy it into
prose** — here or anywhere else. It is longer than the obvious guesses and it
moves. `AGENTS.md`'s gate 5, for instance, routinely tells agents to add an
entry to `scripts/check-sdk-parity-allowlist.txt`, which is on the list; an
agent working from a remembered "it's the governance program and the workflows"
would touch a protected path without noticing.

Touching any path on that list makes `surface-governance` and
`surface-regeneration` fail unless a GitHub commit status exists in the exact
context `nmp/surface-governance-migration`, created by `pablof7z` (immutable user
id 779813) and binding the exact `(PR, base, head, per-path object)` tuple.

## 2. The rule — POLICY

A change that touches a protected path or prefix requires **both**, before it
merges:

1. **Signoff from a completely different agent than the one that wrote it.**
   Not a self-review, and not an agent in the author's own chain — a different
   session that did not produce the diff. The signoff is posted on the PR and
   says what it checked, not merely that it approves.
2. **A note in the `/nmp` mosaico channel tagging Pablo**, so the owner learns
   the rulebook moved without having to read CI:
   `mosaico channel send --channel /nmp --tag Pablo --message "..."`.

Neither substitutes for the other, and neither substitutes for owner
authorization. The commit status remains the designed control; this procedure
covers the window in which its verdict is unenforced (§4), and it is deleted
when that window closes (§4.1).

### 2.1 What the signoff must cover

The gate **aborts at the authorization step**, before the rest of its work. So a
protected-path PR that merges red has not merely skipped authorization — it has
skipped everything downstream of it:

- `surface-governance` runs with `SURFACE_SKIP_REGEN=1`; its abort skips
  change-log validation.
- `surface-regeneration` runs without it; its abort skips deterministic
  double-order regeneration and the committed-snapshot staleness comparison, as
  well as change-log validation.

Note what the abort does *not* skip: the base-trusted falsifier step
(`test-surface-governance.sh`, `test-install-surface-tools.sh`, and the
base-locked `cargo test` runs) executes *before* authorization, so its verdict
on the head is real and already in the job log. Do not re-run those; run the two
above.

To run them, reproduce what `.github/workflows/surface-governance.yml` and
`ci.yml` do — extract the governance program from the **base** commit into a
scratch directory and point it at a worktree checked out at the head, with
`SURFACE_BASE_REF`/`SURFACE_HEAD_REF`/`SURFACE_PR_NUMBER`/`SURFACE_PR_URL` set to
the PR's values. Because the authorization step will abort again, replace its
`verify` call with `migration_status=3` **in the scratch copy only**. That copy
is evidence scaffolding: it is never committed, and the checker in the tree is
never touched (§3).

Record the exact commands and results on the PR. "The other checks were green"
is not evidence about the two that were not.

## 3. There is no retroactive form of this — POLICY

Authorization cannot be granted after a merge, by design.
`check-surface-migration-authorization.py`'s `require_open_pull_request`
requires the referenced pull request to be `state == "open"` and
`merged is False`, and to have `base.sha` still equal to the bound base in the
live API record; `require_current_base` additionally requires the head to be
descended from exactly that base. The status certifies exactly the bytes the
owner looked at, *before they can change*.

So do not propose a post-hoc authorization mode. It would have to drop those
requirements, and what survived would bind nothing a machine can check — a way
to launder an unauthorized merge, which is the failure the program exists to
prevent. When a protected-path change lands unauthorized, the honest remedy is a
durable record plus the by-hand evidence of §2.1, never a manufactured approval.

And never act as the owner identity to create the status. Authorization is
`pablof7z` (779813); the automation identity is `pablof7z-agent` (300045268).

## 4. Why the control is procedural — POLICY

The mechanism itself is sound and is not what failed. Both workflows extract the
governance program from the **base** commit and run that against the proposed
head as inert Git data, so a PR that weakens the checker is judged by the old
checker, which fails closed.

What is missing is enforcement. `master` has **no branch protection at all**
(`GET /repos/pablof7z/nmp/branches/master/protection` → 404, and the repository
has no rulesets), so a red check blocks nothing and the correct verdict can be
ruled advisory and merged through. Installing protection needs repository-admin
authority the automation identity does not have, and is owned by
[#608](https://github.com/pablof7z/nmp/issues/608).

Until #608 lands there is no mechanical stop, so the stop has to be a human one —
and a human stop only works if the human hears about it, which is what the
mosaico note is for.

### 4.1 When this rule is deleted — POLICY

**When #608 installs branch protection requiring `surface-governance` and
`surface-regeneration`, this document and its `AGENTS.md` bullet are deleted in
that same change.** No deprecation, no "keep both for a while"
(`no-backwards-compatibility.md`).

That is not tidiness, it is the point. Under branch protection a protected-path
change cannot merge unless `pablof7z` creates the authorization status himself,
so the owner necessarily knows and the mechanical control necessarily binds.
Keeping a second, human permission gate on top of that would be exactly the
standing manual approval [#592](https://github.com/pablof7z/nmp/issues/592)
rejected. This rule exists *only* for the interval in which the mechanism's
verdict is advisory.

## 5. Why this is not a general review gate — POLICY

#592 was closed on the ruling that mandatory adversarial review is
*governance-by-policing*: bad paths must be excluded by types/APIs plus
executable falsifiers (`docs/bug-class-ledger.md:3-5`), and review is useful
evidence, never merge authority. That ruling stands, and this rule is scoped so
it does not reopen it.

The scoping argument is about what merging through a red *leaves behind*:

- For ordinary code, merging through a red gate does not erase the red. The
  falsifier is still there, it still fails on the next run, and the defect stays
  visible until someone fixes it. Merging early is a delay, not a deletion.
- For a protected path, merging through the red **removes the red**. The
  workflows trust the *base* copy of the governance program, so the moment a
  weakened program lands it becomes the base — the trusted judge for every
  subsequent PR is now the thing that was never authorized, and nothing is red
  any more. The bypass erases its own evidence.

That asymmetry is why the control has to act at merge time and cannot be
deferred to "we will notice later": for this one class of change, there is no
later. It attaches to exactly the paths `PRODUCTION_POLICY` names and to nothing
else, and it ends when #608 makes it redundant (§4.1).

## 6. The incident — 2026-07-31

Two protected-path changes landed on `master` the same day with
`surface-governance` and `surface-regeneration` red on
`no status exists in exact context nmp/surface-governance-migration`:

- `a51933e3` ([#1171](https://github.com/pablof7z/nmp/pull/1171)) changed the
  governance program itself.
- `2d2a14fc` ([#1181](https://github.com/pablof7z/nmp/pull/1181)) added a job to
  `.github/workflows/architecture-gates.yml` and changed
  `tools/behavior-traceability/`.

Both diffs were substantively sound; the missing thing was the control, not the
content. `a51933e3` had its skipped checks re-run by an independent reviewer
after the fact; `2d2a14fc` had not, until
[#1183](https://github.com/pablof7z/nmp/issues/1183) re-ran them. This rule
exists so the next one is signed off and announced before it lands rather than
reconstructed afterwards.
