# Source map

These are the authorities to inspect when a checkout differs from the verified revision. Paths are relative to the NMP repository root. Each `Source:` entry is checked by `scripts/validate_skill.py` when the repo is available.

## Product truth

- Source: `AGENTS.md`
- Source: `README.md`
- Source: `docs/VISION.md`
- Source: `docs/known-gaps.md`
- Source: `docs/builder/28-patterns.md`
- Source: `docs/design/async-observation-handles.md`
- Source: `docs/internals/conventions/bech32-boundary.md`
- Source: `docs/internals/conventions/no-backwards-compatibility.md`
- Source: `docs/internals/conventions/schema-epoch-discard.md`
- Source: `docs/design/durable-write-signing-and-retry.md`
- Source: `docs/builder/15-editing-replaceable.md`
- Source: `docs/internals/writes/payload-and-replaceable-edits.md`
- Source: `docs/builder/08-packaging.md`
- Source: `docs/builder/19-offline-sync.md`
- Source: `docs/builder/22-diagnostics.md`
- Source: `docs/builder/23-threading-lifecycle.md`
- Source: `docs/builder/25-testing.md`
- Source: `docs/builder/30-platform-guides.md`
- Source: `docs/builder/31-gallery.md`
- Source: `docs/builder/32-extending.md`
- Source: `docs/design/protocol-modules-and-composition.md`

## Direct Rust facade and value types

- Source: `crates/nmp/src/lib.rs`
- Source: `crates/nmp/src/engine.rs`
- Source: `crates/nmp/src/engine`
- Source: `crates/nmp/src/error.rs`
- Source: `crates/nmp/src/config.rs`
- Source: `crates/nmp/src/relay_information.rs`
- Source: `crates/nmp/src/subscription.rs`
- Source: `crates/nmp/src/observation.rs`
- Source: `crates/nmp/src/auth.rs`
- Source: `crates/nmp-grammar/src/binding.rs`
- Source: `crates/nmp-grammar/src/descriptor.rs`
- Source: `crates/nmp-grammar/src/live_query.rs`
- Source: `crates/nmp-grammar/src/selector.rs`
- Source: `crates/nmp-grammar/src/tagging.rs`
- Source: `crates/nmp-grammar/src/write.rs`
- Source: `crates/nmp/src/publish_queue/mod.rs`
- Source: `crates/nmp/src/publish_queue/result.rs`
- Source: `crates/nmp/src/core/diagnostics.rs`
- Source: `crates/nmp/src/core/evidence.rs`
- Source: `crates/nmp/src/core/mod.rs`
- Source: `crates/nmp/src/core/write.rs`
- Source: `crates/nmp/src/diagnostics.rs`
- Source: `crates/nmp/src/runtime/mod.rs`
- Source: `crates/nmp/src/runtime`
- Source: `crates/nmp/src/runtime/receipt_stream.rs`
- Source: `crates/nmp/src/relay_information_service.rs`
- Source: `crates/nmp-store/src/lib.rs`
- Source: `crates/nmp-store/src/persistence_failure.rs`
- Source: `crates/nmp-transport/src/thread_census.rs`
- Source: `crates/nmp-nip02/Cargo.toml`
- Source: `crates/nmp-nip02/src/edit.rs`
- Source: `crates/nmp-nip02/src/lib.rs`
- Source: `crates/nmp-nip02/src/service.rs`
- Source: `crates/nmp-nip22/src/lib.rs`
- Source: `crates/nmp-nip22/src/intent.rs`
- Source: `crates/nmp-nip22/src/root.rs`
- Source: `crates/nmp-nip29/Cargo.toml`
- Source: `crates/nmp-nip29/src/lib.rs`
- Source: `crates/nmp-nip29/src/context.rs`
- Source: `crates/nmp-nip29/src/group_list.rs`
- Source: `crates/nmp-nip29/src/operations.rs`
- Source: `crates/nmp-nip29/src/simple_groups.rs`
- Source: `crates/nmp-nip73/src/lib.rs`
- Source: `crates/nmp-signer/src/op.rs`
- Source: `crates/nmp-content/src/document.rs`
- Source: `crates/nmp-content/src/parse.rs`

## Facade protocol doors

- Source: `crates/nmp/src/nip18.rs`
- Source: `crates/nmp/src/nip22.rs`
- Source: `crates/nmp/src/nip25.rs`
- Source: `crates/nmp/src/nip29/mod.rs`
- Source: `crates/nmp/src/nip29/group_list_writes.rs`
- Source: `crates/nmp/src/nip29/group.rs`
- Source: `crates/nmp/src/nip29/groups.rs`
- Source: `crates/nmp/src/nip29/predicate.rs`
- Source: `crates/nmp/src/nip29/read.rs`
- Source: `crates/nmp/src/nip29/records.rs`
- Source: `crates/nmp/src/nip65.rs`
- Source: `crates/nmp/src/nipc7.rs`
- Source: `crates/nmp/src/content.rs`
- Source: `crates/nmp/src/asset.rs`
- Source: `crates/nmp/src/blossom.rs`

## FFI and native wrappers

- Source: `crates/nmp-ffi/src/facade.rs`
- Source: `crates/nmp-ffi/src/facade`
- Source: `crates/nmp-ffi/src/types.rs`
- Source: `crates/nmp-ffi/src/convert.rs`
- Source: `crates/nmp-ffi/src/nip02.rs`
- Source: `crates/nmp-ffi/src/nip22.rs`
- Source: `crates/nmp-ffi/src/nip29.rs`
- Source: `crates/nmp-ffi/src/nip29_simple_groups.rs`
- Source: `crates/nmp-ffi/src/tagging.rs`
- Source: `crates/nmp-ffi/src/content.rs`
- Source: `crates/nmp-ffi/src/asset.rs`
- Source: `crates/nmp-ffi/src/blossom.rs`
- Source: `Packages/NMP/Sources/NMP/Engine.swift`
- Source: `Packages/NMP/Sources/NMP/NMPError.swift`
- Source: `Packages/NMP/Sources/NMP/Query.swift`
- Source: `Packages/NMP/Sources/NMP/Window.swift`
- Source: `Packages/NMP/Sources/NMP/DiagnosticsQuery.swift`
- Source: `Packages/NMP/Sources/NMP/Session.swift`
- Source: `Packages/NMP/Sources/NMP/NMPFilter.swift`
- Source: `Packages/NMP/Sources/NMP/NMPDemand.swift`
- Source: `Packages/NMP/Sources/NMP/Row.swift`
- Source: `Packages/NMP/Sources/NMP/WriteIntent.swift`
- Source: `Packages/NMP/Sources/NMP/Receipt.swift`
- Source: `Packages/NMP/Sources/NMP/Diagnostics.swift`
- Source: `Packages/NMP/Sources/NMP/NostrEntity.swift`
- Source: `Packages/NMP/Sources/NMP/Signing.swift`
- Source: `Packages/NMP/Sources/NMP/Asset.swift`
- Source: `Packages/NMP/Sources/NMP/AuthPolicy.swift`
- Source: `Packages/NMP/Sources/NMP/RelayInformation.swift`
- Source: `Packages/NMP/Tests/NMPTests/DiagnosticsTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/FollowingTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/SigningTests.swift`
- Source: `Packages/NMP/Tests/NMPTests/RelayInformationTests.swift`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Engine.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPError.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Query.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Window.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/DiagnosticsQuery.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Session.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPFilter.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NMPDemand.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Row.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/WriteIntent.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Receipt.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Diagnostics.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NostrEntity.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Signing.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Asset.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/RelayInformation.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/DiagnosticsConcurrencyTest.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/SigningTest.kt`
- Source: `Packages/NMPKotlin/src/test/kotlin/com/nmp/sdk/RelayInformationTest.kt`

## Optional packages and build truth

- Source: `native/features.toml`
- Source: `crates/nmp-cli/src/main.rs`
- Source: `crates/nmp-cli/src/catalog.rs`
- Source: `crates/nmp-cli/src/manifest.rs`
- Source: `crates/nmp-cli/src/prepare.rs`
- Source: `crates/nmp-cli/tests/contracts.rs`
- Source: `Packages/NMP/Package.swift`
- Source: `Packages/NMP/README.md`
- Source: `Packages/NMP/Sources/NMP/Following.swift`
- Source: `Packages/NMP/Sources/NMP/Tagging.swift`
- Source: `Packages/NMP/Sources/NMP/NIP22.swift`
- Source: `Packages/NMP/Sources/NMP/NIP29.swift`
- Source: `Packages/NMP/Sources/NMP/NIP29SimpleGroups.swift`
- Source: `Packages/NMP/Sources/NMP/Blossom.swift`
- Source: `Packages/NMP/Sources/NMPContent`
- Source: `Packages/NMP/Sources/NMPContent/ContentDocument.swift`
- Source: `Packages/NMP/Sources/NMPUI`
- Source: `Packages/NMPKotlin/README.md`
- Source: `Packages/NMPKotlin/build.gradle.kts`
- Source: `Packages/NMPKotlin/settings.gradle.kts`
- Source: `Packages/NMPKotlin/ui/build.gradle.kts`
- Source: `Packages/NMPKotlin/ui/src/main/kotlin/com/nmp/ui`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Content.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Following.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Tagging.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP22.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29SimpleGroups.kt`
- Source: `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/Blossom.kt`
- Source: `Packages/NMPAndroid/README.md`
- Source: `Packages/NMPAndroid/build.gradle.kts`
- Source: `scripts/build-swift-xcframework.sh`
- Source: `scripts/build-kotlin-jvm.sh`

Use public declarations and tests as current API truth. Use `docs/VISION.md` for intended invariants, `docs/builder/28-patterns.md` for what the design structurally excludes, and `docs/known-gaps.md` for what is not built. Comments inside mechanism code can explain design but do not create a consumer API.
