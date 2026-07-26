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

The sibling `Packages/NMPAndroid` project now builds a source-reproducible API
26+ AAR with exact `arm64-v8a` and `x86_64` slices, generated Android UniFFI
bindings, and the same hand-written `com.nmp.sdk` facade. Its qualification
compiles a standalone app against the locally published coordinate and
falsifies missing-ABI and binding/native mismatch controls. A second gate runs
that external consumer on a pinned API-35 x86_64 emulator, receives a real
event and scoped evidence from a host-owned controlled relay through the public
facade, cancels collection, closes/reopens the app-private store, and proves an
arm64-only package refuses native construction (#832). The same external app
now explicitly installs one app-owned engine lifetime and uses an ordinary
Android `ViewModel` for a screen collection. Hosted lifecycle falsifiers prove
that Activity recreation and background/foreground transitions retain that
exact owner and collection, independent cold-flow collections remain exact
handles, final cancellation removes wire demand while preserving cached rows,
and close races leave no collector or late app callback (#833).

A production Android product must also include standard Keystore-backed
providers and prove process-death receipt/signer reattachment (#834), not
merely AAR construction. Newest-state observation is bounded/conflated while
receipt history remains recoverable. The app—not a framework-owned singleton—
owns engine and collector lifetime; #833 proves that boundary without adding
an NMP `Application`, provider, navigation host, or `ViewModel` base class.

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
app. Keystore/process-death recovery and NIP-55 execution remain open work; AAR
construction is qualified by #831, external-consumer runtime by #832, and the
ordinary app-owned lifecycle by #833.

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
