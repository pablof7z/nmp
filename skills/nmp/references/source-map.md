# Source map

These are the authorities to inspect when a checkout differs from the verified revision. Paths are relative to the NMP repository root. Each `Source:` entry is checked by `scripts/validate_skill.py` when the repo is available.

## Product truth and governance

- Source: `README.md`
- Source: `docs/VISION.md`
- Source: `docs/known-gaps.md`
- Source: `docs/bug-class-ledger.md`
- Source: `docs/design/native-task-executor.md`
- Source: `docs/architecture/supported-surface.md`
- Source: `docs/surface-change-log.md`
- Source: `docs/builder/19-offline-sync.md`
- Source: `docs/builder/22-diagnostics.md`
- Source: `docs/builder/23-threading-lifecycle.md`
- Source: `docs/builder/25-testing.md`
- Source: `docs/builder/31-gallery.md`
- Source: `docs/builder/32-extending.md`
- Source: `docs/design/protocol-modules-and-composition.md`

## Direct Rust facade and value types

- Source: `crates/nmp/src/lib.rs`
- Source: `crates/nmp/src/engine.rs`
- Source: `crates/nmp/src/error.rs`
- Source: `crates/nmp/src/config.rs`
- Source: `crates/nmp/src/subscription.rs`
- Source: `crates/nmp-grammar/src/binding.rs`
- Source: `crates/nmp-grammar/src/descriptor.rs`
- Source: `crates/nmp-grammar/src/write.rs`
- Source: `crates/nmp-engine/src/outbox/mod.rs`
- Source: `crates/nmp-engine/src/core/diagnostics.rs`
- Source: `crates/nmp-engine/src/core/mod.rs`
- Source: `crates/nmp-engine/src/runtime/mod.rs`
- Source: `crates/nmp-engine/src/relay_information.rs`
- Source: `crates/nmp-executor/src/lib.rs`
- Source: `crates/nmp-nip02/src/lib.rs`
- Source: `crates/nmp-nip02/src/service.rs`
- Source: `crates/nmp-nip29/src/lib.rs`
- Source: `crates/nmp-signer/src/nip46.rs`
- Source: `crates/nmp-signer/tests/nip46_mock_relay.rs`
- Source: `crates/nmp-ownership/src/lib.rs`
- Source: `crates/nmp-content/src/article.rs`
- Source: `crates/nmp-content/src/reference.rs`

## FFI and native wrappers

- Source: `crates/nmp-ffi/src/facade.rs`
- Source: `crates/nmp-ffi/src/convert.rs`
- Source: `crates/nmp-ffi/src/nip02.rs`
- Source: `crates/nmp-ffi/src/signer.rs`
- Source: `Packages/NMP/Sources/NMP/Engine.swift`
- Source: `Packages/NMP/Sources/NMP/NMPError.swift`
- Source: `Packages/NMP/Sources/NMP/Query.swift`
- Source: `Packages/NMP/Sources/NMP/DiagnosticsQuery.swift`
- Source: `Packages/NMP/Sources/NMP/InsecureFileAccountStore.swift`
- Source: `Packages/NMP/Sources/NMP/NMPFilter.swift`
- Source: `Packages/NMP/Sources/NMP/NMPDemand.swift`
- Source: `Packages/NMP/Sources/NMP/Row.swift`
- Source: `Packages/NMP/Sources/NMP/WriteIntent.swift`
- Source: `Packages/NMP/Sources/NMP/Receipt.swift`
- Source: `Packages/NMP/Sources/NMP/Diagnostics.swift`
- Source: `Packages/NMP/Sources/NMP/NostrEntity.swift`
- Source: `Packages/NMP/Sources/NMP/RemoteSigner.swift`
- Source: `Packages/NMP/Sources/NMP/RelayInformation.swift`
- Source: `Packages/NMP/Tests/NMPTests/DiagnosticsTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/FollowingTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/RemoteSignerTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/RelayInformationTests.swift`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Engine.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPError.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Query.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/DiagnosticsQuery.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/InsecureFileAccountStore.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPFilter.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPDemand.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Row.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/WriteIntent.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Receipt.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Diagnostics.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NostrEntity.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/RemoteSigner.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/RelayInformation.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/DiagnosticsConcurrencyTest.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/RemoteSignerTest.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/RelayInformationTest.kt`

## Optional packages and build truth

- Source: `Packages/NMP/Package.swift`
- Source: `Packages/NMP/README.md`
- Source: `Packages/NMP/Sources/NMP/Following.swift`
- Source: `Packages/NMP/Sources/NMP/NIP29.swift`
- Source: `Packages/NMP/Sources/NMPContent`
- Source: `Packages/NMP/Sources/NMPContent/ContentSession.swift`
- Source: `Packages/NMP/Sources/NMPUI`
- Source: `Packages/NMPKotlin/README.md`
- Source: `Packages/NMPKotlin/build.gradle.kts`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Content.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/ContentSession.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt`
- Source: `scripts/build-swift-xcframework.sh`
- Source: `scripts/build-kotlin-jvm.sh`

Use public declarations and tests as current API truth. Use `docs/VISION.md` for intended invariants and `docs/known-gaps.md`/the bug-class ledger for proof status. Comments inside mechanism code can explain design but do not create a consumer API.
