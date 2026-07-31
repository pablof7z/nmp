---
title: Protected-path changes need a different agent's signoff and a mosaico note
category: conventions
slug: protected-path-signoff
status: policy
date: 2026-07-31
owns:
  - the rule binding any change to a protected governance path or prefix
  - what "a completely different agent" means and what the signoff must cover
  - why this control is procedural rather than mechanical
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
`protected_prefixes`. Today that covers the governance program itself, the
surface tooling, `.github/workflows/{ci,architecture-gates,surface-governance}.yml`,
and `tools/behavior-traceability/`.

**That program is the single source of truth.** Do not copy the list into prose,
here or anywhere else; read it. A copy would go stale exactly when it matters,
and it would be a second spelling of the same fact.

Touching any of those paths makes `surface-governance` and `surface-regeneration`
fail unless a GitHub commit status exists in the exact context
`nmp/surface-governance-migration`, created by `pablof7z` (immutable user id
779813) and binding the exact `(PR, base, head, per-path object)` tuple.

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
authorization where the owner is available to give it — the commit status
remains the designed control, and this procedure is what covers the window in
which it is unenforced (§4).

### 2.1 What the signoff must cover

The gate **aborts at the authorization step**, before the rest of its work. So a
protected-path PR that merges red has not merely skipped authorization — it has
skipped everything downstream of it. The signoff must therefore run by hand what
the aborted gate never machine-checked:

- `surface-governance` runs with `SURFACE_SKIP_REGEN=1`; its abort skips
  change-log validation.
- `surface-regeneration` runs without it; its abort skips deterministic
  double-order regeneration and the committed-snapshot staleness comparison.

Record the exact commands and results on the PR. "The other checks were green"
is not evidence about the two that were not.

## 3. There is no retroactive form of this — POLICY

Authorization cannot be granted after a merge, by design.
`check-surface-migration-authorization.py` requires the referenced pull request
to be open and unmerged, and requires the bound base to still be the branch tip.
The status certifies exactly the bytes the owner looked at, *before they can
change*.

So do not propose a post-hoc authorization mode. It would have to drop both
requirements, and what survived would bind nothing a machine can check — a way
to launder an unauthorized merge, which is the failure the program exists to
prevent. When a protected-path change lands unauthorized, the honest remedy is a
durable record plus the by-hand evidence of §2.1, never a manufactured approval.

And never act as the owner identity to create the status. Authorization is
`pablof7z` (779813); the automation identity is `pablof7z-agent` (300045268).

## 4. Why the control is procedural — POLICY

`master` has **no branch protection at all**
(`GET /repos/pablof7z/nmp/branches/master/protection` → 404, and the repository
has no rulesets). A red check therefore blocks nothing; `surface-governance` and
`surface-regeneration` are advisory in practice, however fail-closed they are by
construction. Installing protection needs repository-admin authority the
automation identity does not have, and is owned by
[#608](https://github.com/pablof7z/nmp/issues/608).

Until #608 lands there is no mechanical stop, so the stop has to be a human one —
and a human control only works if the human hears about it, which is what the
mosaico note is for.

## 5. Why this is not a general review gate — POLICY

[#592](https://github.com/pablof7z/nmp/issues/592) was closed on the ruling that
mandatory adversarial review is *governance-by-policing*: bad paths must be
excluded by types/APIs plus executable falsifiers
(`docs/bug-class-ledger.md:3-5`), and review is useful evidence, never merge
authority. That ruling stands, and this rule is deliberately scoped so it does
not reopen it.

Protected paths are different in kind, and the difference is not a matter of
degree:

- For ordinary code, a falsifier can exclude the bad path. That is the whole
  doctrine, and it is why review is not needed as authority.
- For the governance program, the thing being changed **is** the mechanism. No
  falsifier inside the rulebook can exclude "the rulebook was weakened", because
  the check that would catch it is the check being edited. The designed control
  was never a falsifier; it was owner authorization.

So this rule attaches to exactly the paths that program protects and to nothing
else. Extending it to ordinary code would recreate the reviewer-memory coupling
#592 rejected.

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
