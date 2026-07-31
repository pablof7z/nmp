# NMPNip46

Selectable Swift NIP-46 signer provider for the core `NMP` package. It owns
bunker/invitation parsing, connection/checkpoint lifecycle, provider discovery,
and Primal handoff. The core engine crosses into this package only through the
opaque, take-once `FfiSignerAdapter`; the core privately owns the driver,
installation lease, and exact ordinary signer registration.

From the repository root:

```sh
scripts/build-swift-nip46-xcframework.sh --macos-only
swift test --package-path Packages/NMPNip46
```

The provider builder independently seals the core and provider artifacts,
verifies their shared component-interface identity, and localizes the
provider's private copy of that interface namespace before the two static
archives are linked into one app image.

Each artifact embeds a deterministic v2 identity (`nmp-core-component-v2-*`,
`nmp-nip46-component-v2-*`, and `nmp-component-interface-v2-*`) over
the governed core source, lockfile, compiler, target/profile, feature set, and
selected Cargo graph. `NMPNip46` compares the packaged provider binding,
loaded provider native, packaged interface, and loaded core identities before
it prepares an adapter. A mismatch throws
`NMPError.nativeComponentMismatch(component:expectedIdentity:actualIdentity:)`;
no external Rust object has crossed into the provider at that point. The
four preparation functions require the opaque compatibility proof returned by
that check, and only their constructorless Prepared carrier can reveal an
adapter. The SDK performs prepare→install lexically and retains Prepared plus
the core installation lease together.

Use `--sim-only` or no mode flag when an iOS Simulator or device slice is
needed. Apps that do not add this package neither name nor link NIP-46.
