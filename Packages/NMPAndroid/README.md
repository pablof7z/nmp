# NMP Android AAR

This project packages the existing `com.nmp.sdk` Kotlin facade as an Android
library. It does not fork that facade: Gradle compiles the hand-written sources
from `Packages/NMPKotlin` together with Android-generated UniFFI bindings and
the Rust `nmp-ffi` libraries.

The qualified matrix is deliberately explicit:

| Property | Value |
| --- | --- |
| Android API floor | 26 |
| `compileSdk` | 35 |
| Android Gradle Plugin | 8.7.3 |
| Gradle | 8.10 (the checked-in `Packages/NMPKotlin` wrapper) |
| JDK | 17 |
| NDK | 27.2.12479018 |
| `cargo-ndk` | 4.1.2 |
| ABIs | `arm64-v8a`, `x86_64` |

Build and inspect the AAR from the repository root:

```sh
scripts/build-android-aar.sh
scripts/verify-android-aar.sh \
  Packages/NMPAndroid/build/outputs/aar/NMPAndroid-release.aar \
  Packages/NMPAndroid/src/main/kotlin/uniffi/nmp_ffi/nmp_ffi.kt
```

Run the full packaging proof, including deliberate missing-ABI and
binding/native mismatch controls plus a standalone consumer build:

```sh
scripts/test-build-android-aar.sh
```

The consumer imports `com.nmp.sdk.*`. `uniffi.nmp_ffi` remains generated
implementation plumbing and is not an alternate app API.

Issue #831 qualifies source construction, ABI contents, dependency metadata,
and external compilation. Issue #832 runs that exact artifact through
`com.nmp.sdk` on a pinned API-35 x86_64 emulator: controlled-relay observation,
bounded cancellation, app-private persistent reopen, unavailable-relay scoped
evidence, explicit/idempotent close, and a wrong-ABI control. Issue #833's
external fixture then proves one explicitly app-owned engine across Activity
recreation and background/foreground transitions, exact cold-Flow handle
cancellation, deterministic concurrent close, and zero post-teardown
collectors or wire demand. NMP itself adds no required Android owner/provider
type. Issue #834 adds Android-native account and NIP-46 checkpoint providers:
`NMPAndroidKeyStoreAccountStore` and
`NMPAndroidKeyStoreNip46SessionCheckpointStore`. Each generates a
non-exportable AES-256-GCM wrapping key in `AndroidKeyStore` and stores only
authenticated ciphertext in the app's `noBackupFilesDir`. Missing keys,
invalidated keys, locked-user refusal, corrupt/tampered ciphertext, platform
key refusal, and persistence failures remain typed failures; an absent
checkpoint remains the existing typed `null` state. Same-alias operations are
linearized across provider instances, and `clear()` removes ciphertext before
the wrapping key so stale ciphertext can never silently mint a new identity.

The governed emulator records the wrapping key's actual `KeyInfo` security
level rather than claiming every Android Keystore implementation is
hardware-backed. The AES wrapping key may be software-, TEE-, or
StrongBox-backed. NMP does **not** claim secp256k1 signing happens inside
Android Keystore: restoring either checkpoint decrypts the local account or
NIP-46 client secret into the engine's live memory. The process-death
qualification runs three separate app processes and proves exact local-account
restore, NIP-46 restore without re-pairing, durable receipt reattachment
without republishing, relay ACK completion, clear, and non-resurrection while
the canonical row and receipt remain durable.

The desktop JCEKS/password providers remain excluded from this artifact. The
explicit plaintext development checkpoint remains available at API 26 with
its existing warning; production Android consumers should use the two
AndroidKeyStore providers above.
