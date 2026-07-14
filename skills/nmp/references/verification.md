# Verification

Match evidence to the claim. Compilation proves shape; it does not prove relay acquisition, cancellation, signing, durable recovery, or platform parity.

## Verification ladder

| Tier | Proves | Does not prove |
|---|---|---|
| Static/API inspection | named declarations exist at current revision | runtime behavior |
| App fold test | UI/model interprets public values correctly | engine invariants |
| Scripted async test | bounded lifecycle and status sequencing | store/transport integration |
| Facade integration | canonical store, routing, signing, observation | process-loss recovery unless restarted |
| Crash/restart | atomic acceptance and durable reattachment | public relay interoperability |
| Platform parity | native wrappers preserve Rust semantics | device/package integration unless run there |
| Bounded live smoke | packaging and actual relay/signature path | deterministic exhaustive correctness |

## Consumer workflow

1. Record repo revision and chosen tier.
2. Inspect every named type/method in the supported facade and wrappers.
3. Run the focused compile/test command.
4. Test app reducers with sequences of full row/evidence snapshots and receipt facts.
5. Run a deterministic integration for each feature boundary that relies on NMP behavior.
6. Add restart proof for durable store, receipt, or signer-restoration claims.
7. Add a separately bounded live smoke only when interoperability is part of the claim.
8. Record gaps and environmental skips explicitly.

## Commands

Direct Rust supported-facade proof:

```sh
cargo test -p nmp-consumer-check
cargo test -p <each-touched-crate>
```

Swift from a clean-clone shape:

```sh
scripts/build-swift-xcframework.sh --sim-only
cd Packages/NMP
swift test
```

Use a full/device xcframework and real device when the claim concerns device signing, package integration, or device-only behavior.

Kotlin/JVM:

```sh
scripts/build-kotlin-jvm.sh
cd Packages/NMPKotlin
./gradlew test
```

This does not prove Android AAR, lifecycle, Keystore, package visibility, `Intent`, or Compose behavior.

Repository merge gate after targeted loops:

```sh
cargo test --workspace
```

Follow repository CI and `AGENTS.md`; do not substitute the merge gate for fast touched-crate iteration.

## Query falsifiers

At minimum exercise:

- cached rows before connection and evidence-only updates;
- duplicate event id with provenance growth;
- row removal/replacement;
- source disconnect/reconnect while demand remains live;
- no planned source and local-limit shortfalls;
- last-observer teardown;
- two Kotlin collectors versus one explicitly shared flow;
- pagination/window replacement without a permanent duplicate observation; and
- query/NIP-02 observation refusal with no escaped handle, plus terminal NIP-02 action refusal on the returned `FollowAction`.

Assert rows/evidence, not a fabricated `synced` Boolean.

## Write and identity falsifiers

Exercise the product-relevant subset:

- durable acceptance is visible separately from relay ACK;
- missing signer retains the expected author and resumes only with that identity;
- one ACK plus one rejection remains mixed evidence;
- at-most-once ambiguity never offers blind retry;
- pre-acceptance failure leaves no durable row/receipt;
- native receipt-bridge saturation/OS-thread refusal occurs before acceptance and leaves no obligation or consumed composed intent;
- process restart reattaches the same receipt and frozen intent;
- transient live statuses are not falsely claimed as replayed;
- old remote-signer close cannot detach its replacement;
- secret/account checkpoint is separate from event-store reset;
- cancelling the app receipt consumer neither detaches the native bridge nor cancels the obligation; and
- Kotlin receipt collection remains drained/bounded by ownership policy and repeated reattachment does not accumulate unnoticed bridges.

## Protocol-module falsifiers

Use the checklist in [Protocol authoring](protocol-authoring.md), then compare the same semantic operation through direct Rust and every projected native tier. Compare final unsigned/signed bytes, authority, rows, evidence, receipt facts, and teardown—not merely JSON shape.

## Diagnostics proof

Drive a known demand through a scripted relay and assert semantic records:

- exact wire filter JSON;
- relay and lane;
- subscription and author counts;
- events received by kind;
- coverage interval presence/absence;
- query source evidence and shortfall; and
- transport degradation when induced.

Do not golden-test screenshots or health scores. Do not assert unprojected Rust-only fields from Swift/Kotlin.

## Boundedness

Every async test needs a real deadline/task race. Avoid polling loops with `sleep`. A failed wait must report the missing state and most recent rows/evidence/receipt/diagnostics.

Stress repeated query, diagnostics, follow, signer, receipt attachment, and content-session open/close when resource ownership changes. A successful native lifecycle test must show admitted tasks never exceed `maxNativeTasks`, drive the event-based idle barrier, and show exact executor census `admitted == 0 && running == 0`; it must also show app-level aggregate content/observer counts stay capped. Exercise both `ExecutorSaturated` and `ThreadUnavailable` without panic, preserve no-handle refusal for observations/direct signer setup, and preserve the terminal failure on the deliberately returned NIP-02 action handle.

## Live smoke discipline

- cap every wait;
- use disposable identities and non-sensitive content;
- never print secrets or invitation URIs;
- report relay unavailability as environmental evidence, not a product pass;
- keep live smoke separate from deterministic CI; and
- inspect the running consumer result, not only command exit status.

Complete the [verification record asset](../assets/verification-record.md) so the handoff separates proved, observed, skipped, and unproved claims.
