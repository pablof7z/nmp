# Platform projection contract

Rust owns one semantic product facade. Swift and Kotlin project its values and
behavior into native observation, cancellation, and secure-capability idioms.

For basic code shapes, see [One semantic API, native platform shapes](06-first-app.md).

## What must be identical

- demand identity and printed binding expansion;
- canonical rows and pending/signed identity;
- cache, acquisition, and shortfall evidence;
- signer default, override, pinning, and reattachment;
- protocol-module final unsigned bytes and context provenance;
- durability/receipt facts, including non-durable policy abandonment;
- diagnostics facts and configured limits; and
- bounded slow-consumer behavior.

Native naming and ownership syntax may differ. Semantics may not.

## Rust

An application depends on `nmp`, not the store/router/resolver/transport
mechanism crates. `Engine` owns construction and every invariant shared with
FFI. Test-only mechanism injection is explicitly unstable and feature-gated.

Rust observation uses a blocking/push stream or receiver and `Drop` for
withdrawal. Production examples must not spin on `try_recv` plus a timer.

## Swift

Swift uses `AsyncSequence`, ARC, and optional `@Observable` conveniences. A
view/model's existing task supplies scope. NMP does not add an environment
container or scene-phase coordinator.

The SDK supplies a standard Keychain-backed signer provider. The app owns
identity policy and may attach remote/hardware/custom providers.

Query and diagnostics bridges buffer newest state. Receipt facts remain
reattachable rather than relying on an unbounded `AsyncStream` backlog.

## Kotlin and Android

Kotlin uses cold `Flow` and deterministic `awaitClose` cancellation. The app
chooses coroutine scope, `stateIn`, and Compose/ViewModel structure.

The Android product includes a standard Keystore-backed provider and proves
process-death receipt/signer reattachment, not merely JVM binding generation.
Newest-state observation is bounded/conflated while receipt history remains
recoverable.

## Other platforms

A new projection is a product commitment only after it can preserve:

- native lifetime/cancellation;
- persistent store semantics;
- secure capability storage/reattachment;
- bounded delivery/backpressure;
- module packaging; and
- parity falsifiers.

Serializability alone is not a reason to promise TypeScript, web, TUI, or
another mobile target.

## Parity is behavioral

Generated bindings compiling proves ABI compatibility, not product parity. The
same scenarios must run through direct Rust and every FFI projection and compare
rows, evidence, receipts, diagnostics, and final composed bytes.

---

<sub>[Index](README.md) · Related: [Native platform shapes](06-first-app.md) · [Packaging](08-packaging.md) · [Testing](25-testing.md)</sub>
