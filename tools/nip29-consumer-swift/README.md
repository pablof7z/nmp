# NMP NIP-29 Swift consumer

This is #1202's downstream Swift CLI. Its package imports only the hand-written
`NMP` product; it never imports generated `NMPFFI`. That makes it a falsifier
of the actual application wrapper rather than a second raw-FFI test.

On a macOS host with Xcode, Swift 5.9+, Go, `nak`, `jq`, and the relay harness
requirements, run from the repository root:

```sh
NMP_NIP29_HARNESS_BACKEND=host \
  tools/nip29-consumer-swift/run-capstone.sh \
  /tmp/nmp-nip29-swift-run \
  /tmp/nmp-nip29-swift-evidence
```

The runner first rebuilds the macOS XCFramework and generated UniFFI binding
from the current Rust tree. It then runs the hand-written wrapper through the
same unrestricted two-relay phases as the Rust consumer: follows-derived
discovery, app-selected kinds 9 and 30023, relay-specific metadata, exact
diagnostics, typed per-relay publication outcomes, slow consumption, bounded
window growth, provenance growth after reconnect, persistent offline reopen,
and live recovery. Every query/status/diagnostics handle is explicitly
cancelled and the engine is explicitly shut down; deinit remains only the
safety net.

Croissant and `nak` are fixture infrastructure only. All product claims in
`proof-lines.txt` are values delivered by the public `NMP` Swift wrapper.
