# Distributed-systems testing

NMP coordinates local state, relays, signers, retries, receipts, protocol evidence, and restart reconstruction. Example tests alone will miss failures caused by ordering and partial completion.

Classify the claim before choosing the proof.

## Claim types

| Claim | Meaning | Strong proof pattern |
|---|---|---|
| Safety | A bad consequence never occurs | Property/model test plus adversarial schedules |
| Liveness | Progress eventually occurs under stated conditions | Deterministic clock/deadline and controlled recovery |
| Durability | Accepted facts survive process death/reopen | Close/reopen or kill/reconstruct test |
| Isolation | One identity/source/request cannot borrow another's state | Paired contexts and cross-contamination checks |
| Truthfulness | Public evidence describes what actually happened | Independent witness plus typed facade output |
| Boundedness | Work/memory/routes stay within limits without lying | Stress/property test with explicit shortfall |
| Idempotence | Duplicate input/retry does not duplicate consequence | Replay and duplicate-delivery schedules |

Do not use one happy-path end-to-end scenario as proof of all seven.

## Test state transitions, not delays

Avoid “sleep, then inspect.” Timing sleeps create slow, flaky tests and often assert nothing about the transition that matters.

Prefer:

- deterministic clocks;
- explicit barriers/latches;
- relay witnesses that signal receipt;
- bounded polling for an observable state transition;
- channels controlled by the fixture;
- deadlines expressed in helpers, not feature prose.

A bounded wait is acceptable when the condition is meaningful and diagnostics are retained on timeout. The test should not pass because an arbitrary duration happened to be long enough.

## Restart and reconstruction

Whenever a behavior claims durability:

1. create the obligation through the supported operation;
2. stop at the exact claimed durability boundary;
3. destroy in-memory runtime state;
4. reopen/reconstruct using the supported facade;
5. assert the same public facts;
6. continue the operation and assert no duplication or identity drift.

Reading the database before restart is not proof of reconstruction.

Test distinctions such as:

- accepted versus not yet accepted;
- signed versus awaiting signer;
- attempted versus ACKed;
- retry-eligible versus ambiguous;
- cached rows versus acquisition evidence;
- current session capability versus stale epoch.

## Failure-point testing around durable writes

When two persisted facts must never diverge, test failure at every meaningful point:

- before the first write;
- after the first write but before the second;
- after both writes but before acknowledgement;
- during flush/commit;
- immediately after acceptance returns;
- during reopen/reconciliation.

Then reopen and inspect through the public contract.

Examples:

- a coverage/evidence claim must not exist without the facts that justify it;
- an accepted write must not vanish after returning success;
- a receipt must not report a relay attempt that never occurred;
- cancellation must not leave a displaced replaceable winner permanently hidden.

Use fault injection or child-process termination rather than comments claiming atomicity.

## Reordering and duplication

Exercise schedules such as:

- event before EOSE and EOSE before a delayed event;
- duplicate event or duplicate ACK;
- reconnect before unsubscribe completes;
- signer response after cancellation;
- relay-list update while dependent queries are active;
- account change during discovery;
- restart between signing and first attempt;
- rejection from one relay while another remains pending;
- stale response from an abandoned NIP-77 request.

State-machine/model tests are usually the best primary proof. Add a facade capstone for the user-visible consequence when warranted.

## At-most-once ambiguity

Network loss can make it impossible to know whether a relay accepted a write. Do not test or document this as ordinary failure.

The contract must state what NMP does under ambiguity:

- retry safely because the operation is idempotent;
- retain an ambiguous durable state;
- require caller intervention;
- use an exact event identity that makes duplicate publication harmless.

Tests must reproduce the ambiguity boundary and assert no unsafe duplicate consequence.

## Independent witnesses

For external effects, maintain witnesses outside NMP:

- relay contact/request/event logs;
- signer request/response logs;
- process and thread census;
- filesystem/port ownership;
- platform callback capture.

Use NMP diagnostics to explain behavior, not as the only proof that the external effect did or did not happen.

## Deterministic fixtures versus live probes

### Deterministic required suite

Use scripted local fixtures for product semantics:

- exact events and responses;
- controlled ordering;
- forced disconnects;
- known identities and clocks;
- replayable seeds;
- bounded execution.

### Live opt-in suite

Use public relays/providers only for compatibility questions:

- protocol interpretation in the field;
- provider-specific AUTH/signing behavior;
- deployment/network-policy surprises.

A live pass is not a substitute for deterministic proof. A live failure must retain enough evidence to distinguish product defects from infrastructure drift.

## Reproducibility and failure artifacts

A randomized or scheduled test must emit:

- seed;
- operation sequence or schedule;
- relevant identities and request descriptors without secrets;
- relay/signer witness log;
- facade frames/receipts/diagnostics;
- restart/failure points;
- exact command to replay.

Retain artifacts on failure and remove them on success. Do not “fix” flakes by increasing retries or timeouts before identifying the uncontrolled state.

## Test isolation

Each scenario or fault run owns its:

- temporary store/home;
- relays and ports;
- engine/runtime;
- identities and signers;
- environment variables;
- clock and scheduler controls;
- child processes and threads.

Teardown must prove ownership was released. Leakage between tests invalidates acquisition, restart, and identity evidence.

## Boundedness

When testing pressure or caps, assert all relevant outcomes:

- hard resource bound is respected;
- newest/current state remains observable if that is the contract;
- dropped/coalesced work is diagnosed;
- semantic shortfall is explicit;
- no first-N truncation masquerades as completeness;
- restart does not erase the fact that work was incomplete.

Use stress/model tests rather than a single feature outline with arbitrary counts.

## Distributed claim checklist

For every distributed behavior, ask:

- What is the exact safety property?
- What assumptions permit liveness?
- At what operation is durability promised?
- Which state must be reconstructed rather than merely persisted?
- Which identities, sources, and epochs are isolated?
- Which external effect needs an independent witness?
- Which orderings, duplicates, and crashes could violate the claim?
- Is retry safe under ambiguity?
- Can the failure be replayed deterministically?
- Does the public evidence remain truthful under partial completion?
