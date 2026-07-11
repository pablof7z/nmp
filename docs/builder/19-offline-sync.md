# Offline & sync: negentropy, coverage watermarks, and the limits of replay

**Status: BUILT** — capability-probed negentropy, coverage watermarks, and reconnection replay are live and headlessly tested (`nmp-engine/src/negentropy/mod.rs`, `nmp-engine/src/core/mod.rs`, `nmp-engine/tests/negentropy_live.rs`). One honest scope gap (the liveness-deadline sweep is not yet wall-clock-driven) is noted at the end.

After this chapter you will know when NMP reconciles with a relay using negentropy versus a plain REQ, why that choice is a compile-time property and not a runtime guess, how coverage watermarks let the engine skip windows it has already proven, and — crucially — exactly what reconnection replay does *not* restore.

## Negentropy first — but only against a proven relay

NIP-77 negentropy lets two parties reconcile which events they each hold by exchanging compact range fingerprints instead of streaming everything. It is dramatically cheaper than a plain REQ for a broad, long-lived subscription. But a relay that does not speak NIP-77 will choke on a `NEG-OPEN`, so NMP will only ever send one to a relay it has *proven* supports it.

That proof is a **capability token**, and it is the mechanism behind **bug-class ledger #8 (assuming NIP-77 support)**. The token type is `ProbedRelay`, and it has **no public constructor**. The only places one is ever minted are inside the prober, on a cached `Supported` verdict. The negentropy-open effect requires one:

```rust
NegOpen(ProbedRelay, SubId, ConcreteFilter, String),  // ← first field is the TOKEN
```

A caller holding a bare `RelayUrl` structurally cannot build the argument `NegOpen` demands — there is no widen or coerce path from `RelayUrl` to `ProbedRelay` anywhere in the crate. So an **unprobed relay falls through to a plain REQ by construction**, not by a runtime `if state == Supported` that a future edit could invert. This is the ledger's standard: "Lints are not admissible mechanisms." The fall-through is the type system's, not a check's.

### How the choice is actually made

On every recompile, for each relay's REQ the engine asks two questions (`EngineCore::recompile`):

```rust
let broad = filter.limit.is_none();          // no limit → open-ended sync
match (broad, self.prober.probed(relay)) {
    (true, Some(probed)) => open_neg_session(probed, ...),  // negentropy-first
    _                    => plain REQ,                       // everything else
}
```

Two ways to end up on plain REQ, both correct:

1. **Unprobed relay** — `probed(relay)` is `None`; no token; REQ. When the relay connects, the engine fires an idempotent capability probe (`StartProbe`) — a tiny throwaway kind:0 reconciliation whose *response* (`NEG-MSG` vs `NEG-ERR`) classifies the relay `Supported`/`Unsupported` and caches the verdict forever. A relay is never re-probed once classified.
2. **Bounded fetch** — `filter.limit.is_some()`. A "small exact result" is not what set-reconciliation is for, and `limit` poisons coverage attribution anyway, so a limited filter always stays REQ.

## Coverage watermarks: proving a window so you can skip it

When a REQ (or a completed negentropy reconciliation) reaches EOSE/NEG-DONE, the engine records **coverage**: for the atom's window-erased shape, at that relay, the time interval now proven complete. This is stored as a watermark and surfaced to queries as a type — the difference between "there are no results" and "we don't know yet":

```
Coverage = Unknown | CompleteUpTo(watermark)
```

This is **bug-class ledger #7 (cache-miss treated as empty)**. "Not found" is only constructible from a proven watermark. A cold-start offline read serves cached rows tagged `CompleteUpTo` straight from the persisted watermark — including the authoritative *empty* case (zero rows *and* complete, meaning "we checked, there genuinely is nothing"). The sync planner consults the same watermark before re-fetching: a window already proven complete is not re-requested. Every read example in the manual pairs its rows with a `Coverage` for exactly this reason — see *Coverage: empty vs unknown (the trust chapter)* for the read-side treatment.

Coverage advances can flip a query's state with **no new rows at all**: an EOSE that proves a window complete moves the handle from `Unknown` to `CompleteUpTo` and the engine refreshes the handle so your UI can stop showing a spinner. The diagnostics surface exposes per-(filter, relay) coverage directly (`FilterCoverage`), so you can watch a window go from unknown to proven.

## Reconnection replay — and its precise limits

When a relay reconnects, it comes back on a fresh generation with no memory of your subscriptions. The engine replays them (`on_relay_connected`):

1. Clear the relay's stale coverage attribution (the old generation's in-flight EOSE bookkeeping is meaningless now).
2. Re-send every currently-planned REQ for that relay on the new generation (`Effect::Replay`), re-recording each one's send-time attribution snapshot so coverage can be re-proven.
3. Re-probe capability *only if* the verdict was not already cached — a relay proven `Supported` before the drop stays `Supported`.

So your live reads survive a reconnect transparently: the subscriptions come back, coverage re-accrues, watermarks advance again.

What replay **does not** restore is the load-bearing part of this chapter. Replay re-establishes *demand*; it does not resurrect *in-flight transient state*.

- **Ephemeral gaps are gone for good.** An `Ephemeral` write (typing indicators, presence, NIP-42 auth) is fire-and-forget: no receipt, no ack tracking, forgotten the moment it is sent (see *Writing: intents, receipts, and the durability guarantee lattice*). If the socket dropped while an ephemeral event was in flight, nothing replays it — by design. There is no durable record to replay from.
- **In-flight negentropy sessions die silently with the connection.** A reconciliation open against a relay that disconnects is *not* re-opened as a fallback REQ — there is nothing left to `NEG-CLOSE`, the socket is already gone. The relay's `Supported` verdict stays cached, and the *next* recompile naturally re-opens whatever demand still wants that shape. So reconciliation resumes, but the specific in-progress exchange is discarded, not continued.
- **At-most-once writes are never blindly retried.** A relay disconnecting before it acked a pending write is a terminal `GaveUp` for every write still waiting on it — *not* a retry. No durability class re-sends here. This is the `AtMostOnce` amendment to ledger #9: an idempotent RPC (an NWC payment, say) must never be blind-retried across a reconnect, because "did it happen?" is answerable only by reading the receipt stream, never by resending. `Durable`'s stronger tracking buys you an *accurate receipt*, not automatic resend.

The mental model: **replay restores what you are still asking for; it does not restore what was mid-flight when the wire broke.** Cold-start offline is a first-class feature — you open the app on a plane and get cached rows with honest coverage. Reconnection is transparent for durable reads. But anything transient (ephemeral sends, an in-progress reconciliation round, an unacked at-most-once write) is *at-most-once* across the boundary, and the types make that visible rather than pretending otherwise.

## A worked trace

```
1. connect wss://relay.example   → StartProbe (throwaway kind:0 NEG-OPEN)
2. relay replies NEG-MSG          → prober caches Supported
3. broad $myFollows demand recompiles
   → filter.limit == None, probed() == Some(token)
   → NegOpen(token, sub, filter, initial_hex)   [not a REQ]
4. reconciliation exchanges NEG-MSG rounds
5. reconciler returns Done        → backfill REQ for exactly the missing ids
6. backfill EOSE                  → RecordCoverage(key, relay, interval)
   → query flips Unknown → CompleteUpTo(watermark)
7. relay drops                    → session discarded silently, verdict stays Supported
8. relay reconnects               → Replay the current REQ on the new generation
                                     (probe NOT repeated — Supported is cached)
```

Every one of those steps is an `Effect` you can observe headlessly, which is how the negentropy path is tested without a live relay.

## Gaps to know

- **The 30-second liveness-deadline sweep is real but not yet self-firing.** If a negentropy session opens and the relay never replies, `EngineCore::tick` will abandon it in favor of a plain REQ after 30s — but nothing currently drives `Tick` on a wall-clock cadence (D8 forbids a poll-loop timer thread, and no `Handle` verb exists for it yet). The sweep is unit-tested against a synthetic clock; wiring it to fire live is a small follow-up. In practice, a reconnect (step 8) re-establishes the subscription regardless.
- **Already-open plain REQs are not retroactively upgraded to negentropy** at the moment a probe succeeds; the next demand-driven recompile is what routes that shape onto negentropy.

---

<!-- nav-footer -->
<sub>← [Tracing demand](18-tracing-demand.md) · [Index](README.md) · [Capabilities](20-capabilities.md) →</sub>
