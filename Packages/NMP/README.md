# NMP (Swift package)

The Swift SDK boundary (M4). `NMP` is the only target a consuming app
imports: `import NMP; let nmp = try NMPEngine(config: .init(...))`, then
`for await batch in nmp.observe(filter)`. See `Sources/NMP/Engine.swift` for
the full public surface and `Package.swift`'s header comment for how the
three targets (`nmp_ffiFFI`, `NMPFFI`, `NMP`) relate.

## Building from a clean clone (#18)

`Package.swift` declares a `binaryTarget` at `NMP.xcframework` and a
generated-bindings target (`NMPFFI` / `Sources/NMPFFI/nmp_ffi.swift`).
Neither is committed (see `.gitignore`) -- both are build output of the
Rust `nmp-ffi` crate, and committing a binary xcframework would make the
Swift SDK's actual proof-of-correctness (that it's built from the source in
this repo) unverifiable.

That means `swift build` / `swift test` in this directory do **not** work
straight after `git clone` until the artifacts exist once. Generate them
from the **repo root**:

```sh
scripts/build-swift-xcframework.sh --sim-only
```

This cross-compiles `nmp-ffi` for the iOS simulator (arm64 + x86_64,
lipo'd into one fat slice) and macOS (arm64), runs `uniffi-bindgen` in
library mode to generate `Sources/NMPFFI/nmp_ffi.swift`, and assembles
`NMP.xcframework`. It needs no code-signing identity, so it works in CI /
sandboxes with no signing setup. Takes a few minutes on a cold `cargo`
cache. `--sim-only` skips the `aarch64-apple-ios` (physical device) slice;
drop the flag to build that too if you need to run on a real device (needs
a signing identity).

Once that's done, the ordinary commands work from this directory:

```sh
swift build
swift test
```

Re-run `scripts/build-swift-xcframework.sh` after any change to `nmp-ffi`'s
public UniFFI surface (new/changed exported types or methods) -- the
generated bindings and the compiled staticlib both need to stay in sync
with the Rust source.

CI proves this exact path from a clean checkout on every push/PR (see
`.github/workflows/ci.yml`'s `swift-package` job) -- it fails loudly if
`NMP.xcframework` or `Sources/NMPFFI` are ever needed but missing, so this
can't silently regress again.
