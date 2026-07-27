# NMPNip46

Selectable Swift NIP-46 signer provider for the core `NMP` package. It owns
bunker/invitation parsing, connection/checkpoint lifecycle, provider discovery,
and Primal handoff. The core engine crosses into this package only through the
opaque `FfiSignerMailbox`; the provider attaches through the ordinary signer
capability door.

From the repository root:

```sh
scripts/build-swift-xcframework.sh --macos-only
scripts/build-swift-nip46-xcframework.sh --macos-only
swift test --package-path Packages/NMPNip46
```

Use `--sim-only` or no mode flag when an iOS Simulator or device slice is
needed. Apps that do not add this package neither name nor link NIP-46.
