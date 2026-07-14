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

The standard Keychain-backed signer provider remains tracked work. The app owns
identity policy and may attach remote/hardware/custom providers. For explicit
personal/development opt-in, `NMPInsecureFileAccountStore(fileURL:)` provides
plaintext app-sandbox autologin without placing secret material in the Rust
event/outbox store; pass it to `NMPEngine` and call
`clearPersistedAccount()` before destroying the live signer on sign-out.

For the currently built local remote-signer path, add `primalconnect` to the
host app's `LSApplicationQueriesSchemes`, call
`NMPLocalSignerDiscovery.installed()`, start `oneClickConnectNip46`, and wait
for `.ready`. `UIApplication.open` returning `true` is not readiness.

Query and diagnostics bridges buffer newest state. Receipt facts remain
reattachable rather than relying on an unbounded `AsyncStream` backlog.

## Kotlin and Android

Kotlin uses cold `Flow` and deterministic `awaitClose` cancellation. The app
chooses coroutine scope, `stateIn`, and Compose/ViewModel structure.

The optional desktop-JVM `:ui` child now proves controlled relay identity
composables against the public SDK without adding Compose to the core module.
It owns no engine, HTTP, timer, polling, cache, or image loader and is not an
Android artifact qualification; see [Controlled relay identity UI](36-relay-ui.md).

The Android product must include a standard Keystore-backed provider and prove
process-death receipt/signer reattachment, not merely JVM binding generation.
Newest-state observation is bounded/conflated while receipt history remains
recoverable.

The current JVM projection also exposes `NMPInsecureFileAccountStore(Path)` for
explicit plaintext sandbox persistence. It provides the same restore/clear
semantics as Swift and the same warning: it is not Keystore or a secure Android
production provider.

The current desktop-JVM projection can already consume Android package-query
results through `installedAndroid(packageIds)` and produce an exact
`NMPAndroidSignerHandoff(uri, packageName)`. A real Android host must declare
package visibility for the signer packages/schemes, start
`connectNip46(invitation)` before launching the URI, and apply
`Intent.setPackage(packageName)` so a shared scheme never selects the wrong
app. Android AAR/runtime Compose/Keystore and NIP-55 execution remain open work.

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
