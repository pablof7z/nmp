# Troubleshooting & FAQ

**Status: BUILT** (every answer here is read off the real diagnostic surface — anchored to `Packages/NMP/Sources/NMP/Diagnostics.swift` and the field-to-question mapping in *[Diagnostics & debugging](22-diagnostics.md)*.)

After this chapter you'll be able to diagnose the four problems every NMP app eventually hits — empty feed, stuck publish, wrong relays, high CPU — by reading the diagnostic surface rather than guessing. NMP debugging is *reading*: what you asked (per relay), what arrived (per kind), what coverage was proven. Keep a diagnostics screen in your app permanently and these questions answer themselves.

## The master move

Almost every "it's not working" reduces to one of a handful of diagnostic reads. Open the stream once and keep it:

```swift
for await snap in engine.observeDiagnostics() {
    // snap.relays: [RelayDiagnostics]  (relay, wireSubCount, authorsServed,
    //   byLane, filters, eventsByKind, coverage)
    // snap.uncoveredAuthorCount, snap.droppedMergeRules
}
```

Now walk the table.

## The question → diagnostic → fix table

| Symptom | Read this | What it means | Fix |
|---|---|---|---|
| **Feed empty, `relays` is `[]`** | `snapshot.relays.isEmpty` | Nothing was routed at all — the engine planned no subscription. | Your binding resolved to no demand. Confirm you called `setActiveAccount`; confirm a `Reactive`/`Derived` binding actually expanded (an empty follow list → zero authors → nothing to route). See below. |
| **Feed empty, `uncoveredAuthorCount > 0`** | `snapshot.uncoveredAuthorCount` | The engine wanted to route for N authors but knows no write relay for them — the atom has nowhere to go. | Configure indexer relays at construction so the engine can self-discover write relays; wait for kind:10002 discovery to resolve. Never hard-code the authors' relays (there's no `relays:` param). |
| **Feed empty, coverage is `.unknown`** | `relay.coverage` entries | You asked correctly; the engine simply hasn't *proven* completeness yet — this is "still syncing," not "nothing exists." | Wait, or show a spinner. Empty+`Unknown` ≠ empty+`CompleteUpTo`. Distinguish them in your UI (*[Coverage: empty vs unknown](11-coverage.md)*). |
| **Feed empty, `eventsByKind` has your kind with count 0** | `relay.eventsByKind` | The relay was asked and answered with nothing. Genuinely empty (for that relay). | Correct behavior. If you expected results, your *filter* is wrong — check `relay.filters`. |
| **Feed empty, but `eventsByKind` shows count > 0** | compare `eventsByKind` vs your UI | Events arrived; the engine did its job. The bug is downstream of delivery — in *your* fold/sort/filter. | Diagnostics just exonerated NMP. Debug your consumer loop, not the engine. |
| **Wrong/missing posts, filter looks off** | `relay.filters` (exact wire JSON) | The `REQ` the engine actually sent. A `Derived` author-set that expanded wrong shows here. | Compare wire JSON to what you declared; fix the binding (*[Tracing demand through the compiler](18-tracing-demand.md)*). |
| **Publish stuck / never confirms** | the `Receipt.status` stream | The write's real state — `accepted → signed → routed → sent → acked`, or `awaitingCapability`, `gaveUp`, `failed`. | Read the terminal state. `awaitingCapability` = no signer; `failed(no active signer)` = read-only account. See below. |
| **"Wrong relays"** | `relay.relay` + `relay.byLane` | Which relays, and *why* each (which lane routed there). | You never pick relays; the engine did. `byLane` explains it. If it's genuinely wrong, it's a coverage/outbox question, not a config one (*[Relays: outbox, indexers, and roles](17-relays.md)*). |
| **High CPU during load** | `wireSubCount` + your consumer | A large feed yields a full snapshot per delta; an O(n) sort per batch during a burst burns CPU. | Coalesce batches on your side — dropping intermediate snapshots is safe (*[Cost & performance](24-performance.md)*). |
| **High CPU / relay drops, `wireSubCount` huge** | `snapshot.relays[].wireSubCount` | Too many open subscriptions. | You're observing more distinct demand than you need. Let unused query handles go out of scope — teardown is refcount-driven. |
| **`droppedMergeRules` non-empty** | `snapshot.droppedMergeRules` | A widen-only merge rule couldn't prove it widens, so its filters shipped as separate `REQ`s (graceful degradation). | Informational — correctness is preserved; you just paid for extra subs. Rarely actionable from the app. |

## The four in depth

### "My feed is empty" — empty vs unknown

This is the flagship question and it has *three* distinct empty states, and the diagnostic surface is the only way to tell them apart:

1. **Empty because nothing routed** (`relays == []`). You never asked anyone. Cause is almost always identity or an unresolved binding — no active account, or a follow-list-derived query where the follow list itself hasn't loaded, so it expands to zero authors and there's nothing to route.
2. **Empty because unroutable** (`uncoveredAuthorCount > 0`). You asked, but the engine doesn't know where those authors write. Give it indexer relays and let discovery resolve.
3. **Empty because still syncing** (`coverage == .unknown`). You asked the right relays; they just haven't proven completeness yet. This resolves itself.

Only when `eventsByKind` shows a real count *and* your UI is still empty is the bug yours. That single read — "did events actually arrive?" — is the fork between "NMP problem" and "my problem," and it's why you keep diagnostics in the app.

### "Publish is stuck"

A publish is never "stuck" — it's in a state, and the receipt stream tells you which. Read to the terminal:

```swift
for await status in receipt.status {
    switch status {
    case .awaitingCapability:  // no signer available — you have no active keyed account
    case .failed(let reason):  // e.g. published while active on a read-only account
    case .routed(let relays):  // enqueued to these; not yet acked — this is normal in-flight
    case .acked(let relay):    // this relay confirmed
    case .gaveUp(let relay):   // this relay never confirmed; engine stopped retrying
    default: break
    }
}
```

The two common "stuck" causes: **`awaitingCapability`** means there's no signer for the active account (you're browsing read-only — *[Identity & multi-account](16-identity.md)*); **`failed`** on a read-only account means the same thing reached a terminal. Neither is a hang — enqueue is not convergence (bug-ledger #9), so a `Receipt` that only ever reached `.routed` is *in flight*, working as designed, not stuck. For an `Ephemeral` intent the stream may simply finish with nothing further, which is also correct.

### "It's talking to the wrong relays"

You cannot configure NMP to the *right* relays because you never configure relays at all — there is no `relays:` parameter anywhere. So "wrong relays" is really a question about the engine's routing, and `relay.byLane` answers it: each relay's subscriptions are attributed to the lane that put them there (NIP-65 write, hint, indexer discovery). If a relay you didn't expect appears, `byLane` tells you which mechanism routed there — usually an author's own advertised write relay, discovered live. If an author's posts are missing, check that the author appears in some relay's `filters` and that `authorsServed` covers them redundantly (the engine targets more than one relay per author). The fix is never "pass the right relay" — it's understanding coverage (*[Offline & sync](19-offline-sync.md)*).

### "High CPU"

Two shapes, distinguished by `wireSubCount`:

- **`wireSubCount` normal, CPU spikes during load.** You're paying the full-snapshot-per-delta cost in your consumer during a burst — every batch rebuilds and re-sorts the whole array. Coalesce: take only the latest snapshot per frame. Dropping intermediate batches is always safe because each one is already the complete picture (*[Cost & performance](24-performance.md)*).
- **`wireSubCount` large and climbing.** You have more live demand than you need — probably query handles you never released. Teardown is ownership-driven, so let handles go out of scope (or `cancel()` explicitly); the wire sub closes when the last observer of that demand drops.

## FAQ

**Do I need to poll diagnostics?** No. It's pushed reactively — every recompile and every EOSE-driven coverage change yields a fresh snapshot. NMP never polls anywhere (D8). Just `for await`.

**Why does my diagnostics screen show data before any relay connects?** The first snapshot is delivered immediately on registration — it reports the true current state, which early on is an empty plan. That's correct, not a bug.

**My unit test can't reproduce the empty feed.** Reproduce it deterministically by injecting a fake engine that scripts the exact snapshot — an unroutable author, an `.unknown` coverage, a zero `eventsByKind` — and assert your UI's three empty states. See *[Testing an app that embeds NMP](25-testing.md)*.

**Where's the log level / verbose flag?** There isn't one, and you don't need it. The diagnostic surface *is* the log — except it's structured, real (never estimated), and safe to leave on in production. Ship the screen; read the answer.

---

<!-- nav-footer -->
<sub>← [Testing](25-testing.md) · [Index](README.md) · [The batteries: recipes](27-recipes-and-choosing.md) →</sub>
