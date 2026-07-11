# Editing replaceable state safely: the wiped-follow-list trap

**Status: PARTIAL.** What's built today: the store keeps only the current winner of a replaceable event, reads by address return that winner, and you can read-then-write with the tools from the *Writing* and *Coverage* chapters. What is **not** built: a one-call `follow()` / safe-edit *mechanism* that refuses to write from a stale or `Unknown` base. That mechanism is a likely future [bug-class ledger](../bug-class-ledger.md) entry; until it lands, this chapter teaches the safe *pattern* you assemble by hand and marks the sharp edge honestly.

After this chapter you'll understand the single most damaging bug in Nostr clients — *the client that wiped my follow list* — why a naïve `follow()` re-creates it, and the coverage-aware read-modify-write pattern that avoids it today. Read *Coverage: empty vs unknown* first; this chapter depends on it.

## The trap

Replaceable events — kind:0 profiles, kind:3 contact lists, kind:10002 relay lists, the whole 30000–39999 range — have destructive write semantics. A new kind:3 doesn't *append* to your follow list; it **replaces** it wholesale. The newer `created_at` wins (lexically-smallest id breaks ties), and the old event is gone.

That makes every edit a read-modify-write, and read-modify-write over a network is where clients bleed. The canonical disaster:

1. App opens on a cold cache. It hasn't synced your kind:3 yet.
2. You tap "follow" on someone.
3. The app reads your contact list, sees **nothing** (it hasn't arrived), builds a new kind:3 containing *just that one person*, and publishes it.
4. That event has a fresh `created_at`, so it wins. Your 800 follows are now gone — everywhere, permanently, because every relay dutifully superseded the old list with the new one-entry list.

The user did nothing wrong. The app read an *absence* as an *empty list* and wrote from it. This is [ledger #7](../bug-class-ledger.md) — *cache-miss treated as empty* — turning into data loss the moment it feeds a replaceable write.

## Why a naïve one-call `follow()` is unsafe

It is tempting to want this:

```swift
try await nmp.follow(pubkey)   // ⚠️ this does not exist — and here's why
```

A helper that small has to do the read-modify-write *inside itself*, which means it has to answer, invisibly, the hardest question in the whole flow: **is the base I'm editing actually the current winner, and do I know it's complete?** If it reads the local store and finds nothing, does it treat that as "you follow no one" (and wipe you) or as "I don't know yet" (and refuse)? A convenience that hides this decision hides *exactly* the decision that causes the bug. That's why NMP deliberately does **not** ship `follow()` today, even though it ships the filter and the write mechanism it would be built from. Per the batteries boundary (*What NMP does NOT do*), a helper may encode a *protocol fact* but must never silently make a *correctness decision* the caller can't see. Safe replaceable edit is a correctness decision. Until the engine can carry it *structurally* — refusing the write itself when the base is unproven — a helper would just be the wiped-follow-list bug in friendlier packaging.

So the honest state of the world: **the safe edit is a pattern you assemble, and its safety currently rides on your discipline, not on a type that stops you.** Flag that. It is the reason a mechanism-level fix (below) is on the roadmap.

## The safe pattern you write today

Three rules, all of which come straight from chapters you've already read:

**1. Base the edit on a PROVEN read.** Read your current contact list *and its coverage*, and only proceed if coverage is `CompleteUpTo` — i.e. the engine can prove the winner you're holding is really the winner. If coverage is `Unknown`, you do not have a base; you have an absence. Refuse to write.

**2. Edit the winner, not a blank.** The store already returns only the current winner for a replaceable address (ledger #1 — there is no way to read a stale one). Modify *that* event's tags; never construct a fresh list from scratch.

**3. Publish durably and read the receipt.** A list edit is exactly the "loss is user-visible" case from *Writing* — publish `.durable`, watch the receipt stream, and don't tell the user "followed" until a relay acks.

In Swift:

```swift
func addFollow(_ target: String, as me: String) async throws {
    // 1. PROVEN read: current kind:3, with coverage.
    let listQuery = NMPFilter(kinds: [3], authors: .literal([me]))
    let base = try await firstProvenSnapshot(nmp.observe(listQuery))
        // firstProvenSnapshot: iterate until coverage == .completeUpTo(_),
        // then return that RowBatch. Throws StaleBase if it can't be proven.

    guard let current = base.rows.first else {
        // Zero rows AND proven-complete == you genuinely follow no one.
        // Zero rows and Unknown would have thrown above — that's the trap.
        return try await publishList(tags: [["p", target]], as: me)
    }

    // 2. Edit the WINNER's tags. Preserve everything already there.
    var tags = current.tags.filter { !($0.count >= 2 && $0[0] == "p" && $0[1] == target) }
    tags.append(["p", target])

    // 3. Durable write, read the receipt (see the Writing chapter).
    try await publishList(tags: tags, as: me)
}

func publishList(tags: [[String]], as me: String) async throws {
    let intent = WriteIntent(
        pubkey: me,
        createdAt: UInt64(Date().timeIntervalSince1970),
        kind: 3, tags: tags, content: "",
        durability: .durable, routing: .authorOutbox
    )
    let receipt = try await nmp.publish(intent)
    for await status in receipt.status {
        if case .acked = status { return }
        if case .failed(let r) = status { throw NMPError.publishFailed(r) }
    }
}
```

The `guard`/`throw` structure is the whole point. The unsafe app takes the `guard let current = base.rows.first` else-branch on a cold cache and writes a one-entry list. The safe app never gets there, because `firstProvenSnapshot` **throws on `Unknown`** — it distinguishes "no follows" (proven, zero rows) from "not synced" (unproven), which is precisely the distinction *Coverage: empty vs unknown* exists to preserve. `empty ≠ unknown` is not a nicety here; it is the difference between a correct edit and a wiped account.

The Rust shape is identical in spirit — subscribe to the kind:3 query, wait for a `RowBatch` whose `Coverage` is `CompleteUpTo(_)`, edit the winning event's tags, and `handle.publish` a `Durable` `AuthorOutbox` intent, reading terminals off the returned `Receiver<WriteStatus>`.

## What's actually built vs. the intended mechanism

Be precise about the seam:

**Built and load-bearing today**
- The store keeps only the current winner; reads by replaceable address return it and nothing stale (ledger #1).
- Reads carry `Coverage` as a type (`Unknown` vs `CompleteUpTo(watermark)`), so a proven read is *distinguishable* from an absence (ledger #7). This is the hook the whole pattern hangs on.
- Durable publish with a per-relay receipt stream, so you can confirm the edit landed (ledger #9, *Writing*).

**Not built — the safe-edit mechanism (intended shape)**
The pattern above works, but nothing *stops* an app that skips step 1. The structural fix is an **edit intent that states its base** — it carries the winner's id plus the proven coverage watermark it was built from, and the engine **fails the write typed** (a `staleBase` / `unknownBase` terminal, never a silent wipe) if that base is no longer the current winner or if coverage was `Unknown`. In that world the wiped-follow-list bug becomes *unrepresentable*: you cannot construct a replaceable edit without a proven base, the same way you already cannot construct a read that confuses empty with unknown. That's a likely future ledger entry (append-only, mechanism-not-lint, per this repo's discipline). A one-call `follow()` becomes blessable **only after** that mechanism exists — at which point the helper inherits the guarantee instead of hiding its absence.

Until then: assemble the pattern by hand, always read coverage before a replaceable write, and treat `Unknown` as "refuse," never "empty." If your edit code has a code path that builds a replaceable event from `rows.first` without first proving coverage, you have written the wiped-follow-list bug. There is, for now, no type that will stop you — so this is the one place in the manual where the honest instruction is *be careful*, and the honesty is itself the pressure to close the hole structurally.

---

<!-- nav-footer -->
<sub>← [Writing: intents & receipts](14-writing.md) · [Index](README.md) · [Identity & multi-account](16-identity.md) →</sub>
