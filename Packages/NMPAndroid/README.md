# NMP Android

This is the checked Android project template used by `nmp prepare`. It is not a
second Kotlin SDK: the preparation command copies this platform packaging and
materializes the exact Cargo-resolved `com.nmp.sdk` sources, generated UniFFI
binding, and `libnmp_ffi.so` ABI slices selected by the application's
`.nmp.toml`.

From the repository root:

```sh
nmp --manifest native/examples/android-core.toml prepare \
  --output /tmp/nmp-native-android
```

The Android product is API 26+, uses compile SDK 35 and NDK 27.2.12479018,
and contains exactly `arm64-v8a` and `x86_64`. The generated output includes a
release AAR and a local Maven repository. Applications import only
`com.nmp.sdk`; `uniffi.nmp_ffi` is implementation plumbing.

The catalog-pinned side-by-side NDK under the Android SDK is authoritative.
`NMP_ANDROID_NDK_HOME` may name that exact revision explicitly; a generic
`ANDROID_NDK_HOME` exported by a runner cannot silently select another NDK.

The project template cannot assemble alone after a clean checkout because the
feature-selected sources and native libraries are deliberate generated inputs.

The consumer fixture's committed core-only `.nmp.toml` treats the generated
Maven artifact as an external API-35 application would: constructing
`NMPEngine`, a controlled live observation, scoped failure/recovery,
structured cancellation, fresh-process app-private-store reopen, deterministic
close, wrong-ABI refusal, and bounded 64-collector performance -- never using
generated bindings or an app-owned relay socket.
