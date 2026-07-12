---
sketch: 001
name: exhaustive-ios-gallery
question: "Can catalogue, component-studio, and routing-proof surfaces feel like one native app?"
winner: null
tags: [ios, gallery, components, evidence, content]
---

# Sketch 001: Exhaustive iOS Gallery

## Design Read

An interactive native-app prototype for NMP maintainers and app developers,
using a premium editorial iOS language and SwiftUI/HIG interaction patterns.

- Design variance: 6
- Motion intensity: 5
- Visual density: 6
- Palette: neutral mineral surfaces with one signal-orange accent
- Shape rule: soft cards, smaller nested controls, pill action buttons
- Motion rule: feedback and state transitions only, with Reduced Motion support

## Design Question

Can a very broad NMP UI catalogue remain visually desirable while also proving
component states, app-owned overrides, and NMP's real routing boundary?

## How to View

```sh
open .planning/sketches/001-exhaustive-ios-gallery/index.html
```

The literal full proposed inventory, including the exact 84-entry NDK parity
ledger, is in [`CATALOGUE.md`](CATALOGUE.md). The HTML exposes the same 994
unique candidates through its searchable in-phone catalogue.

## Variants

- **A: Live Catalogue** - browse a visually rich component library with mixed
  Nostr content and an optional evidence sheet.
- **B: Component Studio** - inspect one component's variants, states, anatomy,
  and app-owned renderer override.
- **C: Proof Journey** - replay the no-hint address story from source text to a
  rendered article through two configured indexers and a discovered outbox.

## What to Look For

- Whether the Gallery remains inviting despite the catalogue's breadth.
- Whether evidence feels attached to the thing it explains instead of becoming
  a separate developer console.
- Whether the distinction between live production intent and scripted sketch
  state is impossible to miss.
- Whether the same component language works for compact inline content,
  full-width cards, readers, actions, and fallback states.

## Honesty Boundary

This HTML is an interaction sketch with illustrative data and relay evidence.
It does not connect to NMP or Nostr relays. The production SwiftUI Gallery must
replace every illustrative trace with public NMP snapshots and diagnostics.
