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

This issue qualifies source construction, ABI contents, dependency metadata,
and external compilation. It does not claim that the engine has run on Android;
the governed emulator proof is issue #832. Android lifecycle ownership is #833,
and Android Keystore/process-death recovery is #834. Accordingly, the two
desktop JCEKS/password providers are excluded from this artifact. The explicit
plaintext development checkpoint remains available at API 26, with its existing
warning; no production Android credential-security claim is made here.
