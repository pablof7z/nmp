---
title: Requested behavior is active by default — no hidden runtime feature flags
category: conventions
slug: no-hidden-runtime-feature-flags
status: policy
date: 2026-08-13
owns:
  - the rule that requested behavior ships active on the normal runtime path
  - the distinction between runtime activation gates and legitimate configuration
  - the narrow conditions under which staged or optional behavior is valid
related:
  - docs/internals/conventions/no-backwards-compatibility.md
issues:
  - https://github.com/pablof7z/nmp/issues/1420
---

# Requested behavior is active by default — no hidden runtime feature flags

## 1. The rule — POLICY

When the user requests product behavior, ship it active on the normal runtime
path. Never make the user discover and set an extra activation switch such as:

- `ENABLE_X=1` or another environment variable;
- a config boolean whose only job is to keep the behavior off;
- a rollout toggle;
- an `experimental` switch;
- an undocumented opt-in.

Do not add such a gate to hedge implementation risk, preserve the previous
behavior, make a change easier to merge, or avoid updating callers and tests.
If the requested behavior is not ready to be active, the work is not ready to
merge. Keep the issue or PR open instead of merging dormant code.

Tests must exercise the normal path. A suite that proves the behavior only
after enabling an unrequested switch does not prove the user's request was
fulfilled.

## 2. What this rule does not prohibit — POLICY

This is a rule about **runtime activation gates**, not every use of the word
"feature" or every configuration value. It does not prohibit:

- Rust/Cargo compile-time features used to select packaged components;
- ordinary configuration that supplies a required resource or choice, such as
  relays, providers, credentials, endpoints, or an explicitly requested mode;
- a staged rollout, kill switch, or genuinely optional capability when the
  user or an owning product/design decision explicitly requires it.

An exception must be part of the requested behavior or an existing owning
decision. Developer caution is not an implicit rollout decision. Do not invent
one during implementation.

## 3. Proposal and review tells — POLICY

Stop and correct a proposal or change when:

- the user asked for behavior, but the default path still does the old thing;
- documentation tells the user to set an extra variable to get the requested
  result;
- tests enable a switch that production leaves disabled;
- the flag was not requested and has no owning product/design decision;
- "safer rollout" is offered as a reason without an actual rollout plan or
  requirement.

Do not present "active by default vs behind a flag" as a routine implementation
choice. The second option exists only when staged or optional behavior is part
of the requirement.
