# NMP Builder Manual

NMP is an embeddable Nostr sync-and-routing engine, not an application
framework. Apps declare live queries and write intents, consume native reactive
streams, and keep their own state and presentation. Core is content-agnostic;
opt-in NIP modules own exact protocol schemas and operations.

The manual distinguishes **CURRENT**, **PARTIAL**, and **TARGET** behavior.
Public shapes are provisional but governed: changes require evidence,
cross-surface impact review, human signoff, synchronized projections, and
removal of the superseded path.

## Start Here

- [The mental model](02-mental-model.md): the ownership boundary and two nouns.
- [Current Falsifier timeline](04-ten-minute-timeline.md): a real Swift example,
  explicitly one NIP-02/kind:1 fixture rather than a preferred content model.
- [What works today](03-status-map.md): current versus target status and glossary.

## Orient

- [01 - Why NMP exists](01-why-nmp.md)
- [02 - The mental model](02-mental-model.md)
- [03 - Current and target status](03-status-map.md)

## Get Running

- [04 - Current Falsifier timeline](04-ten-minute-timeline.md)
- [05 - The two nouns and ownership](05-two-nouns.md)
- [06 - Small app shapes](06-first-app.md)
- [07 - Brownfield adoption](07-brownfield.md)
- [08 - Packaging and distribution](08-packaging.md)

## Read

- [09 - Binding grammar](09-binding-grammar.md)
- [10 - Consuming snapshots](10-consuming-results.md)
- [11 - Cache and per-source evidence](11-coverage.md)
- [12 - Collection observation mode](12-collection-mode.md) (target)
- [13 - Delivery transforms](13-delivery-transforms.md) (target)

## Write

- [14 - Durable intent, pending rows, and receipts](14-writing.md)
- [15 - Replaceable edits under scoped evidence](15-editing-replaceable.md)

## Hard Concerns

- [16 - Reactive identity and signer selection](16-identity.md)
- [17 - Compiled routes and typed protocol context](17-relays.md)
- [18 - Tracing demand](18-tracing-demand.md)
- [19 - Offline, reconnect, and acquisition evidence](19-offline-sync.md)
- [20 - Signer, crypto, and AUTH capabilities](20-capabilities.md)
- [21 - Provenance and private routes](21-provenance.md)

## Operate

- [22 - Permanent diagnostics](22-diagnostics.md)
- [23 - Threading and bounded delivery](23-threading-lifecycle.md)
- [24 - Cost, coalescing, and limits](24-performance.md)
- [25 - Testing an embedding app](25-testing.md)
- [26 - Troubleshooting from evidence](26-troubleshooting.md)

## Reference

- [27 - Reusable declarations and protocol operations](27-recipes-and-choosing.md)
- [28 - Guarantees and bug classes](28-patterns.md)
- [29 - What NMP does not own](29-not-do.md)
- [30 - Platform projections](30-platform-guides.md)
- [31 - Falsifier gallery](31-gallery.md)
- [32 - Exact protocol modules and composition](32-extending.md)
- [33 - Governed provisional surface](33-versioning.md)

Authoring rules live in [the design guidelines](000-design-guidelines-and-toc.md)
and [governed writing brief](001-writing-brief.md). The repository README and
canonical design contracts take precedence over historical examples.
