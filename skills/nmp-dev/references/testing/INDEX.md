# NMP testing

Use four owners:

- feature files: behavioral meaning;
- tests: executable proof;
- GitHub issues: temporary work;
- design docs and the bug-class ledger: mechanism and class-level rationale.

Do not duplicate one owner's content in another. Current BDD syntax, status,
and runner rules live in [`docs/bdd/000-bdd-approach.md`](../../../../docs/bdd/000-bdd-approach.md).

## User corrections

Treat behavioral nuance as a specification change:

1. Find the owning feature and `Rule`.
2. Correct wrong text; add the smallest contrast that preserves the new axis.
3. Set truthful status.
4. Add evidence at the narrowest contract owner.
5. Prove the named defect or removed mechanism makes that evidence fail.
6. Link unfinished work to an issue.

Never leave the correction only in chat, a PR, or a test name.

## Load only what applies

| Need | Read |
|---|---|
| Scenario meaning, metadata, corrections | [`behavior-specification.md`](behavior-specification.md) |
| Status, evidence, falsifiers | [`evidence-and-traceability.md`](evidence-and-traceability.md) |
| Test owner or layer | [`test-placement.md`](test-placement.md) |
| Routing, acquisition, identity, source context | [`routing-and-context.md`](routing-and-context.md) |
| Restart, faults, timing, ambiguity | [`distributed-systems.md`](distributed-systems.md) |
| Final review | [`review-checklist.md`](review-checklist.md) |

## Done

- Feature meaning and status are accurate.
- Evidence is at the correct owner and fails under its falsifier.
- Fixtures do not inject the result.
- Relevant context, restart, failure, and platform dimensions are covered.
- Evidence locators resolve and pass.
- Remaining work has one issue.
