# NMP (Swift package)

The Swift SDK boundary. `NMP` is the raw two-noun engine target:
`import NMP; let nmp = try NMPEngine(config: .init(...))`, then
`for await batch in nmp.observe(filter)`. `NMPContent` is an optional product
for source-ranged mixed-content parsing, typed kind:0/NIP-23 resources, and
bounded live reference sessions over that same engine. Importing or linking
`NMPContent` is never required to use `NMP`. `NMPUI` is the optional SwiftUI
product: `Avatar`, `Name`, mentions, event chrome, portrait and Medium-style
article cards, user cards, reactions, a live NMP-owned NIP-02 follow button,
and `NostrContent` with locally scoped renderer overrides.

Following is an NMP action rather than button-owned protocol logic:

```swift
let action = nmp.follow(pubkey)
for await status in action.status { /* acquisition + receipt state */ }

let following = try NMPFollowing(engine: nmp, target: pubkey)
NMPFollowButton(following: following)
```

The resource derives truth from NMP's canonical kind:3 query and the action
preserves the exact list under an atomic base precondition. The SwiftUI view
only renders that state and forwards taps. The ordinary action refuses a
missing contact-list base; first-list creation is intentionally a separate,
not-yet-shipped policy rather than a hidden one-contact replacement.

NIP-22 comment composition remains a protocol-owned free function:

```swift
let intent = try commentIntent(
    root: root,
    parent: parent,
    authorPubkey: author,
    createdAt: timestamp,
    content: text,
    correlation: correlation
)
let receipt = try await nmp.publish(intent)
```

The composer returns the ordinary `WriteIntent`. It does not live on
`NMPEngine`, introduce a `CommentIntent` wrapper, or add another publication
lifecycle.

An engine owns one complete session. Apps may persist the opaque exported
payload without interpreting account or provider restoration material:

```swift
let nmp = try NMPEngine(config: config, sessionPayload: storedPayload)
let account = try nmp.session.add(privateKey: secretKey, makeCurrent: true)
let payloadToStore = try nmp.session.export()
```

Public-key-only accounts use the same `NMPSessionAccount` handle shape. An app
that stores `payloadToStore.bytes` must treat those bytes as sensitive and
restore them as one atomic value; #1398 adds the asynchronous storage policy.

See `Sources/NMP/Engine.swift` and
[`docs/builder/34-content.md`](../../docs/builder/34-content.md).
For the SwiftUI product and live Gallery, see
[`docs/builder/35-swiftui-ui.md`](../../docs/builder/35-swiftui-ui.md).

## Building from a clean clone (#18)

An application checks in one NMP feature manifest and prepares one exact local
package from the repository root:

```sh
nmp --manifest path/to/app/.nmp.toml prepare \
  --output path/to/app/Generated/NMP
```

Add `Generated/NMP/apple` to the app as a local Swift package dependency. It
contains one matching XCFramework, UniFFI generated from that binary, and only
the Swift wrappers selected by Cargo. Re-run prepare after changing the app
manifest, NMP source, or relevant target/toolchain inputs; ordinary Xcode
incrementals consume the generated package without running Cargo. See
[`native/README.md`](../../native/README.md).

This repository's complete-surface qualification template declares a
`binaryTarget` at `NMP.xcframework` and a
generated-bindings target (`NMPFFI` / `Sources/NMPFFI/nmp_ffi.swift`).
Neither is committed (see `.gitignore`) -- both are build output of the
Rust `nmp-ffi` crate, and committing a binary xcframework would make the
Swift SDK's actual proof-of-correctness (that it's built from the source in
this repo) unverifiable.

That means maintainer `swift build` / `swift test` in this directory do **not** work
straight after `git clone` until the artifacts exist once. Generate them
from the **repo root**:

```sh
scripts/build-swift-xcframework.sh --sim-only
```

This cross-compiles `nmp-ffi` for the iOS simulator (arm64 + x86_64,
lipo'd into one fat slice) and macOS (arm64), runs `uniffi-bindgen` in
library mode to generate `Sources/NMPFFI/nmp_ffi.swift`, and assembles
`NMP.xcframework`. It installs any Rust target those slices need onto the
toolchain `rust-toolchain.toml` pins, so there is no separate `rustup
target add` step to get right (#1240) -- installing those targets onto a
different toolchain is what leaves a build reporting `can't find crate for
core`. It needs no code-signing identity, so it works in CI /
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

The fixed all-feature script is repository qualification machinery, not the
app feature-selection workflow.

Building from a clean checkout without the generated bindings or binary
artifacts fails loudly.
