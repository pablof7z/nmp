# Platform projection contract

Rust owns one semantic product facade. Swift and Kotlin project its values and
behavior into native observation, cancellation, and secure-capability idioms.

For basic code shapes, see [One semantic API, native platform shapes](06-first-app.md).

## What must be identical

- demand identity and printed binding expansion;
- canonical rows and pending/signed identity;
- cache, acquisition, and shortfall evidence;
- current-account provider selection, override, pinning, and availability
  resumption;
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

The standard Keychain-backed persistence integration remains tracked work. The
current local-key provider lives in the one whole session. The app exports its
opaque `NMPSessionPayload`, stores that sensitive value atomically using its
platform security policy, and supplies the whole value at the next engine
construction. It never parses or partially updates the payload. Remote and
hardware provider implementations remain future work rather than app-attached
runtime categories.

Query and diagnostics bridges buffer newest state. Receipt facts remain
reattachable rather than relying on an unbounded `AsyncStream` backlog.

## Kotlin and Android

Kotlin uses cold `Flow` and deterministic `awaitClose` cancellation. The app
chooses coroutine scope, `stateIn`, and Compose/ViewModel structure.

The optional desktop-JVM `:ui` child now proves controlled relay identity
composables against the public SDK without adding Compose to the core module.
It owns no engine, HTTP, timer, polling, cache, or image loader and is not an
Android artifact qualification; see [Controlled relay identity UI](36-relay-ui.md).

The Android AAR is built from the same app `.nmp.toml` capability declaration as Swift and
desktop JVM. It packages only the selected `com.nmp.sdk` wrappers and matching
UniFFI contract, with API 26 and exact `arm64-v8a`/`x86_64` slices. A clean
external app resolves that Maven artifact and runs the public facade on a
pinned API-35 x86_64 emulator. The governed test covers one controlled live
observation, scoped pre-connect failure and recovery, structured cancellation,
app-private fresh-process cache reopen, deterministic close, wrong-ABI refusal,
and the declared 64-collector latency/thread/CPU/native-heap bounds.

The supported Android product must still include standard Keystore-backed
storage for the opaque whole-session payload and prove process-death session
restore plus receipt/provider resumption. Newest-state observation is
bounded/conflated while receipt history remains recoverable.

Android configuration lifecycle, Compose capstone, Keystore, NIP-55 execution,
and physical-arm64 qualification remain open work (#833–#836).

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
