# Diagnostics: the permanent proof surface

**Status: CURRENT + TARGET.** The engine, FFI, Swift stream, and Falsifier
screen currently expose exact wire filters, subscription counts, lane and
author coverage, inbound event counts, and current aggregate relay watermarks.
Connection, AUTH, retry, limit, and full source-plan evidence are target
extensions.

After this chapter you will know how to answer "what did NMP actually ask, and
what happened at each planned source?" without inventing a global health score.

## Diagnostics explains; it never controls

Apps declare demand while the engine expands bindings, plans relays, coalesces
compatible work, and syncs. Diagnostics is a read-only projection of those same
compiler, store, transport, and outbox facts. Observing it cannot change
routing, coverage, delivery, or retry.

The current snapshot exposes, per relay:

- exact wire filter JSON;
- open wire-subscription count;
- lane counts and authors served/reverse coverage;
- events received by kind; and
- current per-filter watermark state.

Every value comes from engine state. The wire JSON is the actual serialized
filter, not a reconstruction by the app.

## Swift integration today

`observeDiagnostics()` returns an `AsyncSequence` of full current snapshots:

```swift
struct DiagnosticsView: View {
    let engine: NMPEngine
    @State private var snapshot = DiagnosticsSnapshot()

    var body: some View {
        List(snapshot.relays) { relay in
            Section(relay.relay) {
                LabeledContent("Wire subs", value: "\(relay.wireSubCount)")
                LabeledContent("Authors served", value: "\(relay.authorsServed)")
                ForEach(relay.filters, id: \.self) { json in
                    Text(json).font(.caption2.monospaced())
                }
                ForEach(relay.eventsByKind, id: \.kind) { entry in
                    LabeledContent("kind:\(entry.kind)", value: "\(entry.count)")
                }
            }
        }
        .task {
            for await value in engine.observeDiagnostics() {
                snapshot = value
            }
        }
    }
}
```

The current stream emits an initial snapshot immediately. Rust diagnostics use
a single-slot latest-wins mailbox; Swift additionally frame-coalesces and uses
`bufferingNewest(1)`. A slow screen receives the newest complete local
diagnostic state rather than a growing backlog.

## Reading the current facts

**Was any source planned?** If `relays` is empty, no current wire plan exists.
`uncoveredAuthorCount` explains demand for which the router could not find a
source under its current facts and cap.

**What reached the wire?** `relay.filters` is the exact REQ filter JSON. Compare
it to the declared selection and any printed binding expansion.

**What arrived?** `eventsByKind` counts verified inbound events by relay and
kind. A correct filter with zero inbound events is different from a filter that
was never sent.

**Which planned relay finished its request?** The current relay coverage entry
records `Unknown` or `CompleteUpTo` for that relay/filter. Read the latter only
as a source/window fact. It does not mean the query is globally complete or
that an empty local result is authoritative for all of Nostr.

**How was demand shared?** Wire-subscription count, exact filters, lane counts,
authors served, and reverse coverage show whether compatible demand coalesced
and whether the cap left shortfall.

## Target additions

The permanent target surface also retains:

- descriptor selection, source authority, access context, and plan revision;
- connection generation and connecting/disconnected state;
- AUTH required, selected identity/policy reference, success, rejection, and
  error facts;
- EOSE and negentropy session facts per exact request;
- graph, wire, relay, and result limits plus explicit shortfall reasons;
- ingress pressure, backpressure, and forced-disconnect reasons;
- pending signer obligations and per-relay write attempt/retry facts; and
- dropped intermediate observation-frame counts and history aggregation bounds.

Ordinary query snapshots carry only compact evidence useful to app UX. The raw
relay plan and proof trail remain here.

## A reliable debugging order

1. Inspect whether the descriptor produced a source plan and whether any demand
   is explicitly uncovered.
2. Compare the exact wire filters to the declared selection/binding expansion.
3. Inspect connection and AUTH facts for each planned source.
4. Compare request completion/watermarks with inbound event counts.
5. Inspect local shortfall or limit evidence.
6. If events arrived and canonical rows exist but the UI is empty, inspect the
   app's fold, sort, and presentation policy.

This produces evidence, not a verdict. Diagnostics should never emit
`syncHealth`, `globallySynced`, or a fabricated success score.

---

<!-- nav-footer -->
<sub>← [Provenance](21-provenance.md) · [Index](README.md) · [Threading & lifecycle](23-threading-lifecycle.md) →</sub>
