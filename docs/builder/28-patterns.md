# Patterns & anti-patterns: the guarantees you build on

**Status: BUILT** — this chapter retells the [bug-class ledger](../bug-class-ledger.md) from the builder's seat. Each guarantee is anchored to a ledger entry with a CI falsification proof; where an entry is still `not yet` or `candidate`, this chapter says so.

After this chapter you'll be able to name the concrete Nostr bugs NMP makes *unwritable*, recognize the anti-pattern each one kills, and — most usefully — lean on the guarantee instead of coding defensively around a bug that can't happen.

## How to read this chapter

The old NMP had 46 principles and a lint corpus you were trusted to obey. NMP v2 replaces all of that with a ledger of bugs the *shape of the API* excludes — a wrong program doesn't compile, can't reach the wire, or can't corrupt state. To claim a guarantee holds, someone attempts to write the bug and records why the attempt fails. That means each entry below is not advice; it's a property you can build on, the way you build on a type checker.

Each guarantee follows the same three beats: **the guarantee**, **the anti-pattern it makes impossible**, and **how to lean on it** — what you get to *not* write because of it.

---

## #1 — Stale replaceable event retained

**Guarantee.** There is one mutating store door. Replaceable supersession runs *inside* insert: dedup by id first, then supersede by newest `created_at` (lexically-smallest-id tiebreak). There is no public index or storage setter; a read by address returns only the current winner.

**Anti-pattern it kills.** The client that keeps an old kind:0/kind:3/kind:10002 around and shows stale data half the time because two versions coexist in its store.

**How to lean on it.** Never write "is this the newest version?" logic. When you read a replaceable address, what you get *is* the winner. Don't cache profiles in your own dictionary and reconcile them by hand — that reintroduces the second store the guarantee eliminated. (Proof: `nmp-store` supersede + stale-rejection tests; no public setter.)

## #2 — Lost or leaked subscription

**Guarantee.** Wire subscriptions are derived *only* from the live-query demand set. There is no open-a-`REQ` API. A leak requires a live handle (countable in diagnostics); a loss requires you to drop a handle (your explicit act). Drop → refcount edge → demand withdrawal, debounced.

**Anti-pattern it kills.** The subscription bookkeeping bug: opening `REQ`s you forget to close, or closing one the UI still needs and going silently stale.

**How to lean on it.** Tie your query's lifetime to the natural ownership edge — a SwiftUI `.task`, a Kotlin collection scope, a Rust handle's `Drop` — and stop thinking about subscriptions. You never call "close." When the handle goes out of scope, demand drops. If diagnostics show a wire sub you didn't expect, some handle is still alive; that's a real leak with a visible cause, not a mystery. (Proof: no open-`REQ` API; refcount + drop→withdraw tested. Note: `Drop` currently discards its close-delta headlessly — it reaches the wire at M2.)

## #3 — Wrong-relay routing / manual relay lists

**Guarantee.** There is **no `relays:` parameter** on any read or write. Relay choice is compiler output from lane-typed facts. The only relay input is role-tagged operator config (your indexer set), treated as policy, never as a route override.

**Anti-pattern it kills.** Hardcoding relays, or letting the app pin a subscription to a relay the author doesn't write to — the classic "why am I missing half my follows' posts" bug.

**How to lean on it.** Supply your two indexer relays at construction and never think about relays again. You *cannot* accidentally route to the wrong relay because there is no parameter through which to do it. If you catch yourself wanting to "just add a relay for this query," that's the anti-pattern; the answer is always an author fact the engine already routes from. (Proof: `nmp-router` has no `relays:` input; relay choice is pure compiler output.)

## #4 — Uncapped fan-out

**Guarantee.** The relay set for a demand set is the coverage solver's output. Its cap is a required parameter with an engine default — never an accumulated union of per-author relay lists. No code path connects to a relay outside a solver-produced plan.

**Anti-pattern it kills.** The naive client that unions every follow's relay list and opens 200 connections.

**How to lean on it.** You don't budget connections or dedup relay lists. The solver guarantees a 2-relay-minimum covering set under a cap; the diagnostics screen shows you the actual number. (Proof: solver enforces 2-min + required cap, reports shortfall. Cap is per-skeleton today; tightened to global before multi-kind queries matter.)

## #5 — Dedup or provenance loss

**Guarantee.** Insert merges provenance on duplicate id *before* any other processing; provenance (which relays, when) is a field of the stored row. Ids and signatures are never re-derived post-verification. No API returns an event without retained provenance.

**Anti-pattern it kills.** Showing the same event five times because it arrived from five relays — or throwing away *which* relays had it, which is how you route replies and prove coverage.

**How to lean on it.** You get each event once, with its relay provenance attached. Don't dedup in your view layer. (Proof: provenance merges in place on duplicate insert, both backends; same event from two real relays → one row.)

## #6 — Private-event republish

**Guarantee.** Every explicit route carries a typed provenance class. A route from a private lane admits only *narrowing* overrides — the private route type has **no widen operation**. Unroutable private recipients fail closed (typed error), never fall back to public.

**Anti-pattern it kills.** The catastrophe: a DM or private list item leaking to public relays because a routing fallback widened a private send.

**How to lean on it.** When you use `.privateNarrow([...])`, an empty array is exactly how "unroutable" is expressed, and it fails the whole intent rather than falling back. You cannot construct a widen. Trust it and read the receipt for the typed failure. (Proof: `NarrowOnly<T>` has no widen op; empty private set → whole-intent `Failed`, structurally unable to reach a relay.) See [Provenance, and why private events can't be republished](21-provenance.md).

## #7 — Cache-miss treated as empty (and the inverse over-fetch)

**Guarantee.** Query results carry coverage *as a type*: rows plus `Unknown` vs `CompleteUpTo(watermark)`. "Not found" is only constructible from a proven watermark. The sync planner consults the same watermark before re-fetching a proven window.

**Anti-pattern it kills.** Showing a user an empty screen that should be full — conflating "we haven't synced this yet" with "there is nothing." And its inverse: re-fetching a window you've already proven complete.

**How to lean on it.** *Always* branch on coverage. Empty + `.unknown` means "keep the spinner"; empty + `.completeUpTo` means "authoritatively nothing — show the empty state." The Falsifier's `FeedView` does exactly this. Never treat an empty `rows` array as authoritative on its own. (Proof — the capstone: cold-start offline read serves cached rows as `CompleteUpTo` from the persisted watermark, incl. authoritative-empty.) See [Coverage: empty vs unknown](11-coverage.md).

## #8 — Assuming NIP-77 support

**Guarantee.** Negentropy sync requires a probed-capability token minted only by the prober. An unprobed relay can't be passed to the negentropy path — the parameter type won't accept it; it gets a plain `REQ`.

**Anti-pattern it kills.** Sending a negentropy message to a relay that doesn't speak it and hanging, or silently syncing nothing.

**How to lean on it.** You never choose sync strategy. The engine uses negentropy where it's proven available and REQ everywhere else, automatically. There is no knob to get wrong. (Proof: `ProbedRelay` token has no public constructor; `Effect::NegOpen` requires it.)

## #9 — Enqueue treated as converged

**Guarantee.** A durable write returns a receipt whose status is a *stream* with per-relay terminal acks. No durable publish API returns `void`/`bool`. "Is it sent?" is answerable only by reading receipt states (accepted → signed → routed → sent(relay) → acked(relay)). The write's durability class (`durable`/`ephemeral`/`atMostOnce`) is a typed property of the intent.

**Anti-pattern it kills.** The optimistic UI that says "Sent!" the instant you enqueue, then loses the post because no relay actually acked.

**How to lean on it.** Drive your in-flight UI off the receipt stream. Don't treat the `publish` call returning as success — it means *accepted into the outbox*, nothing more. Watch for `.acked` / `.rejected` / `.gaveUp` per relay. (Proof: durable receipt is a `WriteStatus` stream, first state always `.accepted`, never a bool; live publish to two relays resolves to different terminals.) See [Writing](14-writing.md).

## #10 — Multi-account desync / cross-account leak

**Guarantee.** All account-scoped demand hangs off `Reactive(ActivePubkey)` — there is no second place account context lives. Switch is a root replacement whose resolver-ordered execution closes the old graph (reverse-of-open, exactly-once) *before* opening the new. A stale account's callbacks have no surviving subscription.

**Anti-pattern it kills.** Account B's timeline showing account A's follows because a subscription from the old account survived the switch.

**How to lean on it.** Account switch is one call: `setActiveAccount(pubkey)`. Don't tear down queries by hand before switching, don't thread an account id through your query construction. Set the active account and let the graph re-root. Verify zero leakage on the diagnostics screen — it's the acceptance test made visible. (Proof: `set_active_pubkey` re-root closes old-before-new; no atom mentioning the old pubkey survives.) See [Identity & multi-account](16-identity.md).

## #11 — App owning interest expansion

**Guarantee.** `Derived` bindings resolve *inside* the engine; the query API returns final rows, never the expanded intermediate set. `Selector` is closed. There is no seam through which you can observe, cache, or hand-maintain an expansion (diagnostics show expansions read-only, off the data path). `SetOp(Union|Intersect|Diff)` exists so compound sets like "follows minus mutes" are declarable, not hand-maintained.

**Anti-pattern it kills.** The canonical NDK-era bug: your app watches kind:3, reads the p-tags, and re-issues `REQ`s itself — and gets the diff wrong, or misses a new follow, or leaks across account switch.

**How to lean on it.** Declare `Derived(kinds:[3], authors:[$active] → Tag(p))` as one value and never see the author list. When the follow list changes, the engine surgically re-routes. You write zero expansion code because there's nowhere to put it. (Proof: `Derived`+`SetOp` resolve in-engine; `follows − mutes` re-routes surgically with no app-side expansion.) This is the guarantee half the manual is built around — see [Live queries & the binding grammar](09-binding-grammar.md).

## #12 — Presentation in core

**Guarantee.** The engine emits raw tokens only — hex pubkeys, Unix timestamps, verbatim kind:0. No display helper in the engine; no formatted-string field on any FFI type. Formatting is *unreachable* because the vocabulary to express it is absent, not because a lint forbids it. (Scope: unencrypted content. Encrypted payloads are decrypted by an engine-internal capability co-located with the signer, which still emits raw tokens — decryption is a capability, not a presentation step.)

**Anti-pattern it kills.** A "framework" that bakes one app's truncation/locale/date-format choices into the shared layer — the display-separation rot that hit the old repo 27 times.

**How to lean on it.** Format in your app, always. The Falsifier's `shortHex()` and `formatted(_ unixSeconds:)` live in the *view*, not the engine — that's the intended shape. Don't wait for the engine to hand you a display string; it structurally can't, and that absence is a feature, not a gap. (Proof status: `not yet` — mechanism designed, CI falsification pending a public surface.) See [Consuming results](10-consuming-results.md).

## #13 (candidate) — Late-arriving old event skipped by pagination

**Guarantee (candidate).** Two cursors kept apart by type: a **source cursor** over the durable ingest sequence (so an old-timestamped event arriving late is still ingested and fanned out) and a **presentation cursor** over the ordered row collection (sort-key + id). Paging the view never advances acquisition; acquisition never assumes the view's order.

**Anti-pattern it kills.** An event with an old `created_at` that arrives late gets skipped because your pagination cursor already passed its timestamp.

**How to lean on it.** When the [Collection observation mode](12-collection-mode.md) lands, `loadMore` widens the query's own window and never doubles as an acquisition cursor — so late arrivals still show up. **This is a candidate entry, pending its Tier-A gate at M4;** don't lean on it as shipped yet. Until then, if you paginate by hand, keep your view cursor separate from any since/until you use to fetch.

---

## The meta-pattern

Every guarantee above works because *wrong programs can't be expressed*. That's why this chapter is short on "don't do X": mostly, X is unrepresentable. The two places where a bug *is* still writable — the read-modify-write in [#1's edit path](15-editing-replaceable.md) and the candidate #13 — the manual tells you honestly and shows the safe pattern, and treats that honesty as pressure to close the hole structurally. Build on the guarantees; code defensively only where the ledger admits you still can.

## What to read next

- *[What NMP does not do](29-not-do.md)* — the scope fence these guarantees sit inside.
- *[Diagnostics & debugging](22-diagnostics.md)* — how to *see* each guarantee holding on a live engine.

---

<!-- nav-footer -->
<sub>← [The batteries: recipes](27-recipes-and-choosing.md) · [Index](README.md) · [What NMP does NOT do](29-not-do.md) →</sub>
