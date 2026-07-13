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

NIP-46 remote signers are also projected directly:

```swift
// Host Info.plist: LSApplicationQueriesSchemes = ["primalconnect"]
guard let primal = NMPLocalSignerDiscovery.installed().first(where: { $0.id == "primal" })
else { throw NMPError.invalidSigner("Primal is not installed") }
let connection = try await nmp.oneClickConnectNip46(
    signer: primal,
    relays: ["wss://relay.example"],
    metadata: .init(name: "My App")
)
defer { connection.close() }
for await state in connection.states {
    if case .ready(let pubkey) = state { /* now attached to this engine */ break }
}
```

The library starts the relay listener before opening Primal. OS handoff success
does not mean connected; `.ready` follows only after the NIP-46 handshake and
engine attachment. Generic `nostrconnect://` invitations and pasted
`bunker://` tokens use `nip46Invitation` / `connectNip46` directly. Amber is
Android/NIP-55-only and is not falsely offered as an iOS NIP-46 signer.
`close()` is idempotent; it detaches only that exact session and finishes the
state stream. Dropping the last connection reference has the same effect.

For a personal app that explicitly prefers autologin over Keychain, NMP also
ships a deliberately plaintext file provider:

```swift
let accountStore = NMPInsecureFileAccountStore(
    fileURL: applicationSupport.appendingPathComponent("local-account.nsec")
)
let nmp = try NMPEngine(config: config, localAccountStore: accountStore)
let restoredPubkey = try nmp.activeAccount()
```

With that provider configured, a successful `addAccount` is checkpointed and
the next engine construction restores and activates it. Sign-out calls
`clearPersistedAccount()` before shutting down the credential-owning engine.
This provider is not encrypted, Keychain-backed, Secure-Enclave-backed, or a
production-vault claim.

See `Sources/NMP/Engine.swift` and
[`docs/builder/34-content.md`](../../docs/builder/34-content.md).
For the SwiftUI product and live Gallery, see
[`docs/builder/35-swiftui-ui.md`](../../docs/builder/35-swiftui-ui.md).

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
