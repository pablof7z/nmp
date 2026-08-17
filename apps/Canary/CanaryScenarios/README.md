# Canary scenarios

The Canary's real-relay scenario suite (`docs/internals/canary.md`'s
C1-C18), against a real `strfry` child process. `swift test` from this
directory is the entry point -- no `xcodegen`, no `xcodebuild`, no
simulator, no Xcode project involved anywhere in this path.

```sh
swift test
```

macOS only, deliberately: `RelayLabKit` spawns the relay via
`Foundation.Process`, which the iOS SDK does not expose at all, device or
simulator. The iOS `Canary` app is unaffected by any of this and keeps
compiling as its own thing; this package is not on its dependency path.

## Two prerequisites, both one-time

### 1. The NMP xcframework must already be built

This package depends on the local `NMP` Swift package, whose `Package.swift`
declares a `binaryTarget` at the gitignored `Packages/NMP/NMP.xcframework`.
`swift test` here fails immediately if it does not exist yet -- Swift Package
Manager reports it as a missing binary target before compiling anything, not
a confusing runtime error.

Build it once from the **repository root**:

```sh
scripts/build-swift-xcframework.sh --macos-only
```

`--macos-only` (not the `--sim-only` this repository's other Swift
consumers use) is deliberate here: nothing in this package ever runs on an
iOS simulator or device, so there is no reason to also cross-compile the two
iOS simulator slices `--sim-only` builds. `--macos-only` "prepares only the
host artifact needed by SwiftPM builds and tests that do not run an iOS
target" (`scripts/build-swift-xcframework.sh --help`) -- exactly this case.
See `Packages/NMP/README.md` for what the script does and its full flag set.
Takes a few minutes on a cold `cargo` cache; the required Rust targets are
installed onto the toolchain `rust-toolchain.toml` pins automatically.

Re-run it after any change to `nmp-ffi`'s public UniFFI surface, same as any
other Swift consumer of this package.

### 2. `strfry` must already be built

`apps/Canary/setup-strfry.sh` builds a real, commit-pinned `strfry` binary
into `$RELAY_LAB_CACHE_DIR` (default `~/Library/Caches/nmp-canary-relay-lab`)
-- outside the repository, never vendored.

```sh
apps/Canary/setup-strfry.sh
```

Each scenario locates it there itself and calls
[`XCTSkip`](https://developer.apple.com/documentation/xctest/xctskip) by
name, not a bare crash or a confusing `Process` launch failure, if it is
missing:

```
strfry is not built at <path> -- run apps/Canary/setup-strfry.sh first
```

A scenario deliberately does **not** run this script itself: on a genuinely
clean machine it also needs several Homebrew packages installed first
(`brew install pkg-config libtool openssl zlib lmdb flatbuffers secp256k1
zstd libuv perl`), which is real, sometimes multi-minute cost that
`swift test` should not silently pay as a side effect of running.

## Two scenarios are red on purpose

A full `swift test` is **not** all-green, and that is the honest state
rather than a broken checkout. Two scenarios fail because the defect they
protect against is currently present, and neither has been weakened to make
it pass (`docs/internals/canary.md`: a threshold is not raised, a scenario is
not reshaped):

- `C15NIP42AuthTests` -- NIP-42 AUTH deadlocks against a relay that
  challenges in response to a request, i.e. strfry (#1889). Every
  precondition in the file passes; NMP transmits nothing on the protected
  session. Set `CANARY_KEEP_LOGS=1` to keep the relay work directory and
  read strfry's own frame log.
- `C17RepeatedLifecycleChurnTests.testThreeHundredDistinctObservations...`
  -- engine-lifetime memory grows linearly in distinct filters observed
  (#1846).

Everything else passes. If a scenario other than those two is red, that is a
real regression.

## What this means end to end

Neither prerequisite above is optional, and neither is hidden: skip either
one and the very first `swift test` run says exactly which command to run
and where, rather than failing in a way that looks like a scenario or
product defect. Once both have been run once, `swift test` from this
directory is genuinely the one command -- no project generation, no
simulator boot, no Xcode involved at any point.
