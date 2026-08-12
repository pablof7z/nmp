# Native feature preparation

An app checks in one TOML manifest containing only its compile-time NMP
capabilities:

```toml
schema = 1
features = ["nip29", "nip65"]
```

Prepare the exact Apple package or host Kotlin/JVM module with the generic
command using Python 3.11 or newer:

```bash
scripts/nmp-native prepare \
  --manifest path/to/app/nmp.toml \
  --platform apple \
  --output path/to/app/Generated/NMP
```

Use `--platform kotlin-jvm` for the desktop-JVM qualification artifact, or
repeat `--platform` to produce both under the same output. `--apple-target`
may select a subset of the catalog's Apple targets for a host-only or
simulator-only developer loop. Clean CI invokes the same command and manifest.

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

The cache is content-addressed. Reordering the manifest's feature list does not
change its identity. A changed feature set, relevant source, toolchain, target,
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
NIP-65-selected `normal-client.toml`, `representative-mix.toml` (independent
families plus the real `blossom` to `asset` Cargo feature dependency), and
`all.toml`.
