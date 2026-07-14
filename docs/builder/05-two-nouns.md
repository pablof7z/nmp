# Ownership reference

The [mental model](02-mental-model.md) defines the two nouns. This page is the
quick ownership check to use while designing an app or protocol module.

## Workload surface

| Work | Input | Output |
|---|---|---|
| Live query | closed `Demand = Selection + SourceAuthority + AccessContext` | native stream of rows, cache evidence, acquisition facts, and shortfall |
| Write intent | immutable draft, durability, typed context, optional signer override | receipt facts; durable intents also own a crash-safe pending row and delivery obligation |

Identity inputs, capabilities, diagnostics, and modules configure or explain
those operations. They are not additional app workloads or lifecycle systems.
The governed sign-only call is a bounded capability operation: it returns one
verified event value and deliberately creates neither a live query nor a write
obligation.

## Concern ownership

| Concern | NMP | App | UI/runtime |
|---|---|---|---|
| Canonical Nostr rows and provenance | owns | reads | - |
| Query binding graph | resolves | declares values | observes stream |
| Source and relay plan | compiles from typed authority | supplies operator/protocol policy | - |
| Ordering and ranking | - | owns | renders |
| Formatting and labels | - | owns | renders |
| Current-pubkey value | consumes | owns account UX and supplies value | observes app state |
| Signer material | persists no raw secret | owns identity policy | secure provider may store it |
| Durable accepted obligation | persists | declares durability | observes receipt |
| Non-durable obligation | does not resume after process loss | explicitly chooses weaker policy | can reattach to retained receipt facts |
| Pending row visibility | ordinary store/query path | no overlay | renders row state |
| Relay outcome interpretation | reports facts | decides product policy | renders policy |
| Protocol schema/state | exact opt-in module owns | chooses modules and product policy | renders typed values |
| Diagnostics facts | produces | chooses presentation | renders screen |
| Observation lifetime | refcounts shared demand | owns engine placement | cancels/releases handles |

## Placement test

Ask:

> Would another app or platform have to reimplement this to remain correct?

If yes, it likely belongs in core or in the exact protocol module that owns the
specification. If products can legitimately disagree, it belongs in app code
after delivery.

That puts replaceable semantics, dedup/provenance, source routing, pending-row
promotion, NIP schema validation, and retry in shared code. It keeps feed
composition, ranking, product moderation policy, display names, account labels,
and navigation in the app.

Protocol-defined moderation events and reconstructed moderation state remain
owned by their protocol module. The app owns how that state affects its product
and UI.

## Warning signs

The boundary is drifting if an app must own any of these:

- relay `REQ`/`CLOSE` ids;
- expanded author or relay sets for a derived query;
- a second optimistic row collection;
- a timer that polls engine state;
- signer retry and correlation;
- NIP event encoding/validation for an enabled module; or
- an NMP provider, reducer, navigation model, or scene-phase coordinator.

---

<sub>[Index](README.md) · Related: [Mental model](02-mental-model.md) · [Ten-minute embedding](04-ten-minute-timeline.md) · [Binding grammar](09-binding-grammar.md)</sub>
