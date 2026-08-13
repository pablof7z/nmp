# Distributed-systems testing

Classify the claim before choosing proof.

| Claim | Proof pattern |
|---|---|
| Safety | Property/model test with adversarial schedules |
| Liveness | Deterministic clock/deadline and controlled recovery |
| Durability | Kill/close, reopen, reconstruct |
| Isolation | Paired contexts and contamination checks |
| Truthfulness | Independent witness plus typed output |
| Boundedness | Stress/property test with explicit shortfall |
| Idempotence | Replay and duplicate schedules |

One happy path cannot prove all of these.

## Schedules and waits

Use deterministic clocks, barriers, controlled channels, witness signals, or
bounded polling for observable state. Never use a longer sleep as proof.

Exercise relevant reorderings, duplication, reconnect, stale responses,
identity/source changes, partial relay outcomes, and crashes between durable
phases. Prefer state-machine/model tests; add a facade capstone for a distinct
public consequence.

## Durability and faults

For a durability claim:

1. create the fact through the supported operation;
2. stop at the claimed boundary;
3. destroy runtime state;
4. reopen through the facade;
5. assert public facts;
6. continue and check duplication and identity drift.

When persisted facts must not diverge, fault before, between, during, and after
writes/commit/acknowledgement, then reopen. Reading the database before restart
does not prove reconstruction.

## Ambiguity and witnesses

Model lost acknowledgements explicitly. Assert the documented choice: safe
idempotent retry, durable ambiguity, caller intervention, or harmless duplicate
publication by exact event identity.

Use relay, signer, process, filesystem/port, or platform witnesses for external
effects. Diagnostics explain; they do not self-certify.

## Reproducibility and isolation

Failure output records seed, schedule, scrubbed identities/requests, witnesses,
public frames, fault points, artifacts, and replay command.

Each run owns its store/home, relay/port, runtime, identities, environment,
clock, scheduler, and processes. Teardown proves release.

Use deterministic fixtures for semantics. Use live probes only for field
compatibility; live results never replace deterministic proof.
