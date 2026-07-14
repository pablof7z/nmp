# Controlled relay identity UI

NMP ships a small controlled relay family for SwiftUI and optional desktop-JVM
Compose. These views render state that the application supplies. They do not
hold an engine, fetch NIP-11, poll, reconnect, cache, schedule timers, or load
remote images.

Keep the two inputs separate:

- `NMPRelayInformationState` / `NmpRelayInformationState` wraps one-shot
  relay information as loading, available, or unavailable. An available stale
  snapshot remains visible with `Stale` freshness and its separate last error.
- `NMPRelayRuntimePresentation` / `NmpRelayRuntimePresentation` maps an
  optional, query-scoped `SourceStatus`. It does not claim URL-global health,
  authentication, connection, or reconnect state.

## SwiftUI

Import the optional `NMPUI` product and pass the one-shot result down:

```swift
let information = try await engine.relayInformation(for: relay)
let state = NMPRelayInformationState(information)

NMPRelayListEntry(
    information: state,
    runtime: NMPRelayRuntimePresentation(sourceStatus),
    image: appResolvedImage,
    action: { openRelayDetails(relay) }
)
```

The family also exposes `NMPRelayIcon`, `NMPRelayName`,
`NMPRelayDescription`, and `NMPRelayRuntimeStatus` for custom composition.
Construct `.unavailable(relay:reason:)` when acquisition yields no snapshot;
do not discard an available stale-last-good value merely because it carries a
`lastError`.

## Desktop-JVM Compose

The optional `Packages/NMPKotlin/ui` child project depends on the core desktop
JVM SDK while keeping Compose dependencies out of that core module:

```kotlin
val information = engine.relayInformation(relay)
val state = NmpRelayInformationState.available(information)

NmpRelayListEntry(
    information = state,
    runtime = NmpRelayRuntimePresentation.from(sourceStatus),
    painter = appResolvedPainter,
    onClick = { openRelayDetails(relay) },
)
```

The matching leaves are `NmpRelayIcon`, `NmpRelayName`,
`NmpRelayDescription`, and `NmpRelayRuntimeStatus`. This is a desktop-JVM
Compose proof only. It does not publish or qualify an Android AAR.

## Icon ownership

`advertisedIcon` preserves the exact NIP-11 icon string but neither UI package
dereferences it. Apply application-owned URL trust, privacy, cache, and media
policy, then supply an already-resolved SwiftUI `Image` or Compose `Painter`.
Passing no image uses the deterministic initials fallback.
