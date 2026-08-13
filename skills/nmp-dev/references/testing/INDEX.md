# NMP testing

Put each kind of information in one place:

- Feature files describe behavior.
- Tests prove that behavior.
- GitHub issues track unfinished work.
- Design docs and the bug-class ledger explain why the architecture exists and
  which classes of bugs it prevents.

Do not copy the same rule into several places. Current BDD syntax, status, and
runner rules live in
[`docs/bdd/000-bdd-approach.md`](../../../../docs/bdd/000-bdd-approach.md).

## User corrections

When a user correction changes the meaning of a behavior:

1. Find the owning feature and `Rule`.
2. Fix inaccurate text. Add one contrasting example that shows when the result
   should differ.
3. Set truthful status.
4. Put executable proof in the smallest component responsible for the
   behavior.
5. Deliberately reintroduce the named bug or remove its protection. Confirm the
   linked evidence fails.
6. Link unfinished work to an issue.

Never leave the correction only in chat, a PR, or a test name.

## Load only what applies

| Need | Read |
|---|---|
| Feature meaning, metadata, or corrections | [`behavior-specification.md`](behavior-specification.md) |
| Status, test links, or deliberate-break checks | [`evidence-and-traceability.md`](evidence-and-traceability.md) |
| Where a test belongs | [`test-placement.md`](test-placement.md) |
| Routing, identity, source, or request context | [`routing-and-context.md`](routing-and-context.md) |
| Restart, faults, timing, ambiguity | [`distributed-systems.md`](distributed-systems.md) |
| Final review | [`review-checklist.md`](review-checklist.md) |

## Done

- Feature meaning and status are accurate.
- Executable proof sits with the component responsible for the behavior and
  fails when the named bug is reintroduced.
- Test setup provides inputs instead of inserting the expected result.
- Tests cover every request context, restart, failure, and platform condition
  needed by the claim.
- Every evidence locator resolves to a real check in its required CI job.
- All required checks pass. A live check supplements repeatable evidence; it
  does not replace it.
- Remaining work has one issue.
