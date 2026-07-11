# Diagnostics & debugging: "why is my feed empty?"

**Status: BUILT** ★ (engine + FFI surface landed and live-proven; the Falsifier diagnostics screen ships today. Anchored to `nmp-engine/src/core/diagnostics.rs`, `Packages/NMP/Sources/NMP/Diagnostics.swift`, and `apps/Falsifier/Sources/Falsifier/Views/DiagnosticsView.swift`.)

After this chapter you'll be able to answer "why is my feed empty?" — and every question shaped like it — by *reading* the engine instead of guessing. You'll open the diagnostic stream, read a snapshot per relay, and turn each field into a yes/no answer: is my query routed anywhere? is it complete or just unknown-so-far? is the author covered by more than one relay? Debugging NMP is reading, not printf.

## The one idea: the acceptance test, made visible

NMP's routing is invisible by design — you declare a value, the engine picks relays, coalesces demand, and syncs. That invisibility is the whole point (see *[Relays: outbox, indexers, and roles](17-relays.md)*), but it would be intolerable if you couldn't *see* what it did. So the engine exposes a read-only projection of its own internal planes: per relay, the exact wire filters it sent, how many wire subscriptions are open, how many distinct authors that relay covers, the events it has actually received counted by kind, and each filter's proven coverage.

Every number is read from real engine state — never estimated, never fabricated. The module doc says it plainly: filters are rendered to their *exact* wire JSON (`ConcreteFilter::to_nostr().as_json()`), "literally what was actually asked/proven." And it is strictly an observer: observing diagnostics can never influence routing or delivery. It sits off the data path.

That last property is what makes the surface trustworthy as a correctness tool, which is the deeper reason it exists — more on that at the end.

## Observing the stream (Swift, real)

`observeDiagnostics()` returns an `NMPDiagnostics`, an `AsyncSequence` you iterate exactly like a query. Each element is the current engine-global `DiagnosticsSnapshot` — never a delta; every snapshot is already the full picture. Teardown is deinit-tied: drop the iterator and the Rust observer withdraws itself.

```swift
import SwiftUI
import NMP

struct DiagnosticsView: View {
    let engine: NMPEngine
    @State private var snapshot = DiagnosticsSnapshot()

    var body: some View {
        List {
            Section("Summary") {
                LabeledContent("Relays", value: "\(snapshot.relays.count)")
                LabeledContent("Uncovered authors",
                               value: "\(snapshot.uncoveredAuthorCount)")
            }
            ForEach(snapshot.relays) { relay in
                Section(relay.relay) {
                    LabeledContent("Wire subs", value: "\(relay.wireSubCount)")
                    LabeledContent("Authors served", value: "\(relay.authorsServed)")
                    ForEach(relay.filters, id: \.self) { json in
                        Text(json).font(.caption2.monospaced())
                    }
                    ForEach(relay.eventsByKind, id: \.kind) { kc in
                        LabeledContent("events kind:\(kc.kind)", value: "\(kc.count)")
                    }
                    ForEach(relay.coverage, id: \.filter) { fc in
                        Text(coverageLabel(fc.coverage))
                    }
                }
            }
        }
        .task {
            for await s in engine.observeDiagnostics() {
                snapshot = s                       // @State hop → SwiftUI redraws
            }
        }
    }

    func coverageLabel(_ c: Coverage) -> String {
        switch c {
        case .unknown:       return "coverage: unknown"
        case .completeUpTo:  return "coverage: complete"
        }
    }
}
```

That's the whole integration — one call, iterate, assign to `@State`. This is not a toy: it is a lightly trimmed copy of the Falsifier's real `DiagnosticsView`, and the writing brief's advice stands — **ship a diagnostics screen in your app permanently.** It costs one `AsyncSequence` and pays for itself the first time a feed is mysteriously empty.

The snapshot delivers immediately on registration, before any network: a freshly constructed engine yields a well-formed empty snapshot right away (the engine thread primes the mailbox with the current snapshot before replying). So your screen is never blank waiting on a relay — it shows the true current state, which early on is "nothing planned yet."

## The same stream in Rust

```rust
let (diag_handle, rx) = handle.observe_diagnostics();
// rx.recv() blocks (D8: never a poll loop) until the next snapshot.
while let Some(snapshot) = rx.recv() {
    for r in &snapshot.relays {
        println!("{}  subs={}  authors={}", r.relay, r.wire_sub_count, r.authors_served);
        for (kind, count) in &r.events_by_kind {
            println!("    kind:{kind} received {count}");
        }
    }
}
drop(diag_handle); // withdraws the observer (or just let it fall out of scope)
```

The Rust mailbox is *latest-wins*: a slow consumer sees the most recent snapshot next, never a growing backlog of stale ones. Diagnostics is a recomputed projection, so dropping intermediate snapshots loses nothing (unlike a row stream, where every delta matters). Android (`Flow`) and TS (async iterator) will present the identical value shapes — the field names are defined once at the FFI seam and don't vary per platform.

## Reading it: turning fields into answers

Here is the field-to-question map. Every question about "did my query do what I meant?" is one of these.

**"Is my query routed anywhere at all?"** → Look at `snapshot.relays`. If it's empty, the engine planned *no* subscription for your demand. If `uncoveredAuthorCount > 0`, the engine wanted to route for that many authors but knows no write relay for them yet — the atom has nowhere to go. This is the difference between "I asked and got nothing back" and "I never actually asked anyone." A literal-author query for a pubkey whose write relays are unknown, with no indexer configured, produces exactly this: zero relays, one uncovered author.

**"Did my filter reach the wire the way I meant?"** → Read `relay.filters`. These are the *exact* JSON `REQ` bodies the engine sent. If you declared `kinds:[1]` and the wire filter says `"kinds":[1]`, your binding compiled the way you intended. If a `Derived` author-set expanded to the wrong pubkeys, you'll see it here in the `authors` array — this is where *[tracing demand through the compiler](18-tracing-demand.md)* meets the wire.

**"Is the result complete, or just empty-so-far?"** → Read `relay.coverage`. Each entry pairs a filter's wire JSON with a `Coverage` of `.completeUpTo(watermark)` or `.unknown`. `Unknown` means the engine has not yet proven it has everything — an empty feed with `Unknown` coverage means *keep waiting / we haven't synced*, not *there is nothing*. `CompleteUpTo(t)` means the engine can prove the visible rows are everything up to time `t`. This distinction is a type, not a convention (see *[Coverage: empty vs unknown](11-coverage.md)*) — the diagnostic surface is where you watch it flip from `Unknown` to `CompleteUpTo` as EOSE and negentropy land.

**"Did the events actually arrive?"** → Read `relay.eventsByKind`. This is the one datum the router cannot see on its own: it is this engine's own counter, bumped on every inbound `EVENT` frame. If `filters` shows you asked a relay for `kind:1` but `eventsByKind` has no `kind:1` entry, the relay was asked and answered with nothing — a very different situation from never having asked. If `eventsByKind` shows a healthy count but your feed is still empty, the problem is downstream of delivery, in *your* fold or sort, not in NMP.

**"Is the 2-relay-minimum being respected?"** → For any author you care about, count how many relays' `filters` carry that author's pubkey. The engine solves for a minimal covering set with redundancy — an author should generally be served by more than one relay, and `authorsServed` per relay plus the fan-out across relays lets you confirm no author is riding on a single point of failure. If an author appears on exactly one relay, that's the number to watch when their posts go intermittently missing.

## The debugging loop

The practical loop for "why is my feed empty?" is four reads, in order:

1. **`relays` empty?** → nothing routed. Check that you set an active account and that your `Derived`/`Reactive` binding actually resolved to authors (an empty follow list expands to zero authors → nothing to route). Check `uncoveredAuthorCount`.
2. **`relays` present but `filters` wrong?** → your binding compiled to something you didn't mean. Compare the wire JSON to what you declared.
3. **`filters` right but `eventsByKind` zero?** → you asked correctly and the relays had nothing (or aren't reachable). Coverage will read `Unknown` while probing.
4. **`eventsByKind` non-zero but UI still empty?** → the bug is in your app's delivery-side code, not the engine. Diagnostics has just exonerated NMP.

*[Troubleshooting & FAQ](26-troubleshooting.md)* turns this loop into a compact question→diagnostic→fix table.

## The diagnostic as a first-class correctness tool

Because every number is a real measurement rather than an estimate, the diagnostic surface is not just for *your* debugging — it is how the engine's own tests assert correctness end to end. The live test doesn't just check that rows arrived; it opens the diagnostics stream and asserts that a relay's `eventsByKind` reports a real `kind:1` count matching the rows the query already delivered. A routing regression that silently dropped events, or a fabricated relay that never really served anyone, would surface here as a mismatch between what diagnostics claims and what actually came through.

That is the design bet: a surface that can only ever report true numbers becomes the place where wrong behavior has nowhere to hide. It is the acceptance test made visible — for the engine's authors and for you, using the same screen. Which is exactly why the guidance is to keep it in your app forever, not to bolt it on when something breaks.

---

<!-- nav-footer -->
<sub>← [Provenance](21-provenance.md) · [Index](README.md) · [Threading & lifecycle](23-threading-lifecycle.md) →</sub>
