# Test placement

Put a test in the smallest stable component responsible for the behavior, not
automatically in the crate being edited. Organize feature files by user
behavior. Organize executable tests by the component and kind of proof they
need.

## Three layers

Tests prove behavior at three layers. The layers are complementary: a
public-API system scenario does not replace the owner and headless-engine
tests underneath it, and E2E coverage never substitutes for unit coverage of
the invariant it depends on.

### Owner tests

Prove exact local invariants inside the crate that owns the state: both
directions of every mirrored index, replacement behavior, teardown, and
rejection of states production cannot reach. Fixtures must be built only
through production doors (see below) — never by hand-writing the owner's
internal maps.

### Headless engine scenarios

Prove cross-owner ordering, committed-store propagation, generations,
recovery, cancellation, deadlines, and teardown, driving the engine directly.
No sockets, no wall-clock timing — control clocks and channels instead (see
[`distributed-systems.md`](distributed-systems.md)).

### Public-API system scenarios

Prove a complete promise of the supported API using a small real consumer of
`nmp::Engine`, a temporary Redb, the actual runtime, and deterministic
relay/signer/clock fakes — never live internet relays or uncontrolled data.
Exercise restart and complete query and write flows through the API an app
actually calls, not an internal shortcut.

## Decision table

| What the test proves | Where it belongs |
|---|---|
| One value, parser, codec, or state transition | Unit or table test in the responsible crate |
| A rule across many inputs or operation orders | Property, model, or differential test in the responsible crate |
| One crate working with real collaborators | That crate's integration tests |
| A complete promise of the public Rust API | `crates/nmp/tests/` through `nmp` |
| A readable example across several layers | Canonical feature plus `@acceptance`, when the example adds understanding |
| FFI or native-platform behavior | Shared parity tests plus native Swift and Kotlin tests |
| Compatibility with a public provider or network | Opt-in live check |

Do not list every possible input combination in Gherkin or use public
infrastructure as the sole correctness proof.

## Fixture boundary

Setup may provide stores, clocks, scripted relays, identities, network rules,
injected failures, and test-only constructors approved by the owning crate. It
must not perform the behavior being tested or use private state as proof.

A fixture that hand-writes internal maps is a second implementation of the
logic under test, and it can invent states production cannot reach. Four NMP
test files once hand-wrote six wire-ownership maps to build a 10,000-atom
fixture, including assigning an owner refcount directly — a state no
production path can produce. Build fixtures only through production doors:
the same constructors, writes, and API calls a real caller uses.

For discovery, seed the protocol fact and observe contacts/results. Do not
insert or inspect the resolved route directly.

## Internal proof and public example

Important public behavior may need two tests when each proves something
different:

1. an internal property, model, or integration test that covers the rule; and
2. one test through the public API that proves the result an app sees.

Neither replaces the other. Do not keep Rust, shell, and Cucumber copies of the
same path when they prove nothing different.

## Avoid

- Test placement based only on the changed crate.
- Feature files mirroring crate structure.
- Cucumber for every rule that must hold across many cases.
- One lucky end-to-end operation order as proof of a general rule.
- Private table assertions for a public API promise.
- New test crates when an existing integration target can prove the behavior.

Before adding a test target, state what behavior no existing target can prove.
Show that the new target uses the same public path as an app instead of reaching
through an internal shortcut.
