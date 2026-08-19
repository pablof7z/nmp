# NMP testing

Put each kind of information in one place:

- Feature files describe behavior.
- Tests prove that behavior.
- GitHub issues track unfinished work.
- Design docs and `docs/builder/28-patterns.md` explain why the architecture
  exists and which classes of bugs it prevents.

Do not copy the same rule into several places. Current BDD syntax and runner
rules live in [`behavior-specification.md`](behavior-specification.md).

## Three kinds of test

Three kinds of test are available. Each is good at something different; none
is owed:

- **Owner tests** prove exact local invariants inside the crate that owns the
  state.
- **Headless engine scenarios** prove cross-owner behavior with no sockets and
  no wall-clock timing.
- **Public-API system scenarios** prove restart and complete query/write flows
  through the supported `nmp::Engine` API.

See [`test-placement.md`](test-placement.md) for what each is good for.

## User corrections

When a user correction changes the meaning of a behavior:

1. Find the owning feature and `Rule`.
2. Fix inaccurate text. Add one contrasting example that shows when the result
   should differ.
3. Put executable proof in the smallest component responsible for the
   behavior.

Never leave the correction only in chat, a PR, or a test name.

## Load only what applies

| Need | Read |
|---|---|
| Feature meaning or corrections | [`behavior-specification.md`](behavior-specification.md) |
| Where a test belongs | [`test-placement.md`](test-placement.md) |
| Routing, identity, source, or request context | [`routing-and-context.md`](routing-and-context.md) |
| Restart, faults, timing, ambiguity | [`distributed-systems.md`](distributed-systems.md) |
| Final review | [`review-checklist.md`](review-checklist.md) |

## Done

- Feature meaning is accurate.
- Whatever proof was written sits with the component responsible for the
  behavior.
- Test setup provides inputs instead of inserting the expected result.
- The checks that were written pass. There is no CI, so a check is worth only
  what running it by hand showed.
