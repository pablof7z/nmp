---
slug: upstream-guidance-pr
summary: Use when a defect has appeared more than once, or an agent-written app made a mistake the guidance should have prevented, and the lesson belongs upstream as a pull request.
triggers:
  - I've seen this mistake before.
  - Two apps made the same error.
  - An agent built this and got it wrong.
  - Why does everyone get this wrong?
  - Improve the NMP guidance.
status: draft
created: 2026-07-25
updated: 2026-07-25
---

## Outcome

A merged-ready pull request against the repository this profile ships from that
makes the next app's authors — human or agent — unable to make the same mistake
without noticing. Plus a note recording the pattern so it is not rediscovered from
scratch.

The deliverable is a change to guidance, not a description of a bug.

## Inputs

- At least two independent instances of the defect, or one instance written by an
  agent working from this profile or the `nmp` skill. A single human slip is not
  yet a guidance failure.
- The exact guidance text in force when the defect was written.
- The app owners' position on what may be published, if anything recognizable is
  involved.

## Sources of truth

- The `nmp` skill and its guardrails — the text under audit.
- This profile's standing rulings and `references/`.
- The target repository's `AGENTS.md` for contribution rules.
- The app's git history, closed issues, and reverts for prior instances.

## Approach

1. **Find the second instance before writing anything.** Mine app history, your
   cross-app notes, and closed issues. One occurrence is a finding; a recurrence
   is a guidance failure, and only the second kind justifies a PR.
2. **Reconstruct the belief.** What did the author think was true? A defect
   requiring no false belief is a slip — stop, and file it as an app finding
   instead. A defect requiring a plausible false belief is the real target.
3. **Locate the failing sentence.** Find the rule that should have bound. Classify
   why it did not:
   - *absent* — no rule covered it;
   - *buried* — correct but not where the decision gets made;
   - *ambiguous* — readable two ways, and the wrong way was reasonable;
   - *advisory* — phrased as a suggestion when it needed to be a ruling;
   - *misapplied* — obeyed into the wrong outcome, which is worse than missing
     because it looks handled.
   The classification determines the fix. Do not skip to drafting.
4. **Weigh agent-authored evidence heaviest.** When an agent wrote the defect, the
   reasoning is still legible in the code and the commit. That is a direct readout
   of where the text underspecifies reality. Quote the sentence that failed and
   write what it should say instead.
5. **Draft the smallest binding change.** A sharpened ruling beats a new one; a
   new ruling beats a new reference; a new reference beats prose added to an
   existing one. Add a tell — the observable thing a reviewer greps for — not just
   a prohibition.
6. **Generalize.** Strip the app. Ship the defect class, the tell, the severity
   calibration, and the fix shape.
7. **Follow the target repo's rules**: capture the issue first, work in an
   isolated worktree, one coherent unit per PR, and state an honest reason.
8. **Verify every named symbol exists** in the tree the PR targets before opening
   it.
9. Record the pattern in notes, whether or not the PR lands.

## Judgment gates

- Anything recognizably the app's — code, identifiers, product plans, a case study
  they could be identified from — requires the owners' agreement before it goes
  into a public PR. The generalized defect class does not.
- Secrets never appear in a PR, including in a quoted snippet, fixture, or path.
- If the honest reason is small, say the small thing. An invented principle is
  worse than "two apps made this same mistake."
- If the fix would change what the facade guarantees rather than what the guidance
  says, that is an NMP change, not a guidance change. Escalate instead of drafting
  a rule for behavior that does not exist.

## Output

An issue, then a PR: the failing sentence quoted, the classification, the two
instances in generalized form, the replacement text, and the tell a reviewer can
check. No confabulated rationale.

## Done when

The PR is open and references its issue, every symbol it names exists, nothing
proprietary or secret is in it, and notes record the pattern.

## Failure and recovery

- Only one instance found: file it as an app finding, note the suspicion, and wait
  for the second. Do not upstream a hypothesis as a rule.
- Owners decline publication of their case: upstream the generalized class alone,
  which needs no permission.
- The guidance was already correct and findable: the honest conclusion is that
  this was a slip. Say so, close the loop, and do not open a PR to look diligent.

## Learned preferences

Small honest reasons are preferred over inflated ones. The target repository
explicitly rejects fabricated rationale, and a PR that dresses a plain bug in
invented principle will be read as the confabulation it is.
