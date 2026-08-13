# Distributed-systems testing

First state what kind of promise the test must prove:

| Promise | Test it by |
|---|---|
| Safety | Trying many operation orders in a property or model test |
| Liveness | Controlling the clock and deadline, then testing recovery |
| Durability | Kill/close, reopen, reconstruct |
| Isolation | Comparing two users, sessions, or requests and checking for leaks |
| Truthfulness | Comparing the public result with an independent witness |
| Boundedness | Stressing the limit and reporting when it cuts the result short |
| Idempotence | Replaying the same work and delivering duplicates |

One happy path cannot prove all of these.

## Schedules and waits

Control clocks, barriers, channels, witness signals, or polling deadlines so
the test knows when an observable event occurred. Never use a longer sleep as
proof.

Try every failure or ordering that matters to the promise:

- Reorder or duplicate messages.
- Disconnect and reconnect.
- Deliver an old response after the request changed.
- Change the identity or source.
- Let only some relays respond.
- Crash between durable steps.

Prefer a state-machine or model test when many orders are possible. Add one test
through the public API only when it proves a separate user-visible result.

## Durability and faults

For a durability claim:

1. create the fact through the supported operation;
2. stop at the claimed boundary;
3. destroy runtime state;
4. reopen through the public API;
5. check the public result; and
6. continue the operation and check for duplicates or a changed identity.

When stored facts must stay consistent with one another, force failures before,
between, during, and after their writes, commit, and acknowledgement. Then
reopen the system. Reading the database before restart does not prove that NMP
can reconstruct the state.

## Ambiguity and witnesses

Test the case where an operation may have succeeded but its acknowledgement was
lost. Check the documented response: retry safely, preserve the uncertainty in
durable state for the caller, require caller action, or publish the exact same
event again without changing its identity.

Use relay or signer logs, process state, filesystem or port state, or native
platform output to prove external effects. Diagnostics may explain what NMP
believes happened, but they cannot prove their own claim.

## Reproducibility and isolation

Whenever a test fails, record everything needed to replay it:

- the random seed, when one exists, and the operation order;
- scrubbed identities and requests;
- external observations and public frames;
- injected failure points and saved artifacts; and
- the replay command.

Each test run must have its own store, home directory, relay and port, runtime,
identities, environment, clock, scheduler, and processes. Teardown must prove
that it released them.

Use repeatable fixtures to prove behavior. Use live checks only to confirm
compatibility with real services; live results never replace repeatable tests.
