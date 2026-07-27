# NMPNip46

Selectable Swift NIP-46 signer provider for the core `NMP` package. It owns
bunker/invitation parsing, connection/checkpoint lifecycle, provider discovery,
and Primal handoff. The core engine crosses into this package only through the
opaque `FfiSignerMailbox`; the provider attaches through the ordinary signer
capability door.

From the repository root:

```sh
scripts/build-swift-nip46-xcframework.sh --macos-only
swift test --package-path Packages/NMPNip46
```

The provider builder regenerates both core and provider artifacts in one Cargo
resolution. That is required for the external `FfiSignerMailbox` type; two
independently compiled static archives are not link-compatible merely because
their source versions match.

Each artifact embeds a deterministic `nmp-core-component-v1-*` identity over
the governed core source, lockfile, compiler, target/profile, feature set, and
selected Cargo package set. `NMPNip46` compares the provider's required
identity with the loaded core's plain identity before it asks `NMPEngine` for a
mailbox. A mismatch throws
`NMPError.nativeComponentMismatch(component:expectedCoreIdentity:actualCoreIdentity:)`;
no external Rust object has crossed into the provider at that point. The
provider constructor requires the opaque compatibility proof returned by that
check, so mailbox-first construction is not representable in generated
bindings.

Use `--sim-only` or no mode flag when an iOS Simulator or device slice is
needed. Apps that do not add this package neither name nor link NIP-46.
