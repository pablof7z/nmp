# Android AAR external consumer

This standalone Android application is outside `Packages/NMPAndroid` and
resolves `com.nmp:nmp-android:0.0.0-local` from an explicit Maven repository.
Its application and instrumentation sources import only `com.nmp.sdk`; the
raw `uniffi.nmp_ffi` package is not an alternate API.

Issue #831 uses the fixture as a clean external compile consumer. Issue #832
runs its instrumentation suite on a pinned API-35 `x86_64` emulator against a
host-owned controlled NIP-01 relay reachable through Android's documented
`10.0.2.2` loopback alias. The suite proves:

- the AAR's `x86_64` library loads through `NMPEngine`;
- an ordinary exported launcher Activity installs and starts in the target app;
- an app-private persistent store receives a real controlled-relay event;
- ending `Flow.first` cancels the observation and explicit `close()` shuts the
  engine down idempotently;
- a second engine opens the same store and reads the persisted row in
  `CacheOnly` mode;
- an unavailable relay surfaces scoped source failure evidence;
- cancelling a collection before any required frame completes within a bound;
  and
- an arm64-only negative build cannot construct `NMPEngine` on x86_64.

Issue #833 adds an ordinary consuming-app ownership proof without adding an
NMP framework type. Instrumentation explicitly constructs one engine owner,
installs that exact value at the app's dependency-injection boundary, and lets
an Android `ViewModel` own one cold-flow collection. The governed emulator
proves that the same owner and ViewModel survive Activity recreation and
background/foreground transitions; two independent collections remain two
handles while compatible wire work coalesces; cancelling either handle leaves
its peer intact; cancelling the last leaves the canonical row cached; and
concurrent idempotent close drains query, diagnostics, and receipt collectors
without a late callback into disposed app state.

The clean-checkout hosted command is:

```sh
scripts/test-android-emulator.sh
```

That script expects an already-booted emulator plus the AAR, local Maven
publication, Rust relay binary, and Android tooling prepared by
`.github/workflows/android-emulator.yml`. It captures toolchain/device
identity, instrumentation output, relay requests, APK inventories, and logcat
under `artifacts/android-emulator`.

This fixture does not impose a Compose, navigation, `Application`, or
`ViewModel` base class on NMP consumers. Android Keystore and process-death
credential/receipt recovery remain #834.
