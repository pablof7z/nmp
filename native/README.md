# Native feature preparation

An app checks in one `.nmp.toml` containing only its compile-time NMP
capabilities and stable product inputs:

```toml
schema = 1
capabilities = ["nip29", "nip65"]
products = ["apple"]
```

Install the first-class Rust application tool once from the NMP checkout:

```bash
cargo install --locked --path crates/nmp-cli
```

Then initialize and prepare from a clean application repository:

```bash
nmp init --product apple --capability groups --capability "outbox routing"
nmp prepare --output Generated/NMP
nmp verify --output Generated/NMP
```

`nmp capability list`, `add`, and `remove` edit the same declaration without
adding capability-specific commands. Use `products = ["android"]` for an
Android AAR, or select several products in the same file. `apple_targets` may
select a stable subset of the catalog targets. Clean CI invokes the same
`nmp prepare` command from the committed `.nmp.toml`. The installed binary
remembers the NMP source checkout it was built from; `--source`/`NMP_SOURCE`
selects another checkout explicitly. Python is not used by initialization,
preparation, or generated consumer builds.

`features.toml` is the one machine-readable ownership catalog. It maps stable
app keys to forwarding Cargo features and hand-written SDK sources. The tool
asks Cargo metadata which forwarding features are active, including real Cargo
dependencies, and never contains feature-family branches of its own.

The output contains:

- an XCFramework and UniFFI Swift binding plus only the resolved Swift sources;
- or a host dynamic library, UniFFI Kotlin binding, and only the resolved
  Kotlin sources;
- a generated package/module manifest; and
- `nmp-native-provenance.json`, whose identity covers the source, catalog,
  toolchains, targets, profile, canonical requested/resolved features, relevant
  build environment, and output hashes.

The cache is content-addressed. Reordering the manifest's capability list does not
change its identity. A changed capability set, relevant source, toolchain, target,
or profile does. The tool refuses unknown/internal keys, unregistered active
Cargo features, missing catalog sources, or overwriting a non-generated output
directory.

An app can consume the generated Kotlin project as a composite build:

```kotlin
// settings.gradle.kts
includeBuild("Generated/NMP/kotlin-jvm")

// the app module's build.gradle.kts
dependencies {
    implementation("com.nmp:nmp-kotlin:0.0.0")
}
```

The generated local coordinate is deterministic and is not a published Maven
version. Its compiled contents and provenance are identified by the separate
content-addressed NMP identity.

Shared hand-written sources may contain generic conditional blocks:

```swift
// nmp-native:if nip65
// declarations valid only when this catalog key is active
// nmp-native:endif
```

The filtering mechanism validates marker keys against the catalog; it has no
knowledge of any particular family.

The checked examples cover the linear build shapes: `core.toml`, the
NIP-65-selected `normal-client.toml`, `representative-mix.toml` (several
independent families selected together), and
`all.toml`; Android-specific examples select the same four shapes without a
second capability graph. `fixtures/native-cli-app/.nmp.toml` is the clean
application declaration selecting groups plus outbox routing.
