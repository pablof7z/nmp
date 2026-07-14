---
title: "Store, Persistence, and Cache"
source: "legacy-NMP forensic recovery"
record_count: 9
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Store, Persistence, and Cache

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-STORE-1426893A18 (rationale)

> "On app start, does registering the FollowListProjection replay the existing kind:3 from LMDB to it" this would be the wrong approach -- the list of people the user follows should happen to be loaded from the cache by the fact that the use of the follow list is attempted to be used -- an explicit warming up of cache from data that should be loaded because something needs it is a hack and hides an architectural problem (we literally already discussed this exact same problem a few days ago and it was supposed to already had been fixed and written in some docs/ file)

*Provider: claude | Session: e6b44a84-8cfc-48b2-863a-58382398b5df | Timestamp: 2026-06-19T12:30:53.455Z*

---

### LNK-STORE-F4D4494AD7 (explanation)

> This also aligns with #3083: feed decisions must not depend on cache luck. Any model that promotes roots or rewrites rows by peeking into the event store is suspect by construction.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-STORE-2B9C9FF91A (rationale)

> it's ok if it re-resolves per lane because all that would mean is that it would fall down back to checking "is there a subscription asking for kind:3, authors:<current-user>?" since there would be because one of the lanes carried it or has requested it, it would just hit that and be a noop, and the data for the three lanes would get instantly resolved to whatever is in the event cache (lmdb) and rehydrated if something new comes in.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T15:10:39.564Z*

---

### LNK-STORE-D593BE4353 (rationale)

> I need you to review the core of how nmp works because I suspect architectural issues all over the place and mixing of concerns -- I was just told there's a gate that decides whether an event goes into the cache only if the author is in the user's follow list -- I was also told there are kind concerns mixed with how optimistic events are published -- there must be exceptions that were added based on LLMs taking instructions too literally.

*Provider: codex | Session: 019eca68-85c6-77e0-b237-e58f6c894f72 | Timestamp: 2026-06-15T08:34:53.076Z*

---

### LNK-STORE-C41426BF3B (rationale)

**Formulation 1 — 2026-05-20T07:55:43.307Z**

> all NMP apps should be offline-first, meaning, I should be able to publish to a relay while I'm offline, and the intent of publishing the event should be persisted in the local cache and publishing those events that could not be published (either because the client or the relay in question is offline) the moment a connection can be established.

*Provider: codex | Session: 019e444a-e301-7f13-b133-efc4eea66155 | Timestamp: 2026-05-20T07:55:43.307Z*

**Formulation 2 — 2026-05-20T07:55:43.307Z**

> I want to be able to interact with apps to my heart content while offline and have all the events I published by synced to the respective destination relays.

*Provider: codex | Session: 019e444a-e301-7f13-b133-efc4eea66155 | Timestamp: 2026-05-20T07:55:43.307Z*

---

### LNK-STORE-F58B047DD8 (explanation)

**Formulation 1 — 2026-07-11T07:33:08.090Z**

> It had real valuable machinery: outbox routing, relay diagnostics, projection revisions, store semantics, transport lessons, no-polling
>      doctrine, Rust-owned domain logic.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

**Formulation 2 — 2026-07-11T07:33:08.090Z**

> NMP owns:
>   - event store
>   - dedup and provenance merge
>   - replaceable/delete/expiry semantics
>   - persistence
>   - coverage watermarks
>   - query binding resolution
>   - relay routing and outbox discovery
>   - REQ coalescing
>   - 2-relay minimum coverage with capped fan-out
>   - negentropy-first sync where probed
>   - write outbox and per-relay receipts
>   - diagnostics as a permanent proof surface

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-STORE-5827990340 (rationale)

> file as a follow up github issue: I don't want the local cache to ever lose events we fetched; I want a way for the app to be able to use all events we've ever used -- is the reason we need to trim because lmdb has everything in memory all the time? what can we do to make it possible to have an unboudned cache? (we don't need to answer that now, just add the research to be done in a github issue and mention it as a follow up on the plan file so we tackle that next)

*Provider: claude | Session: 78b50727-bccd-4088-8493-a07624a4fa83 | Timestamp: 2026-06-15T09:20:09.996Z*

---

### LNK-STORE-A5817016A3 (explanation)

> NMP owns:
>   - event store
>   - dedup and provenance merge
>   - replaceable/delete/expiry semantics
>   - persistence
>   - coverage watermarks
>   - query binding resolution
>   - relay routing and outbox discovery
>   - REQ coalescing
>   - 2-relay minimum coverage with capped fan-out
>   - negentropy-first sync where probed
>   - write outbox and per-relay receipts
>   - diagnostics as a permanent proof surface

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-STORE-5B5CDBAAB3 (rejection)

> > Does anything actually render the "X replied" badge? Retire-vs-rehome for reply-rollup hinges on whether chirp/29er consume RootItem.attribution. That's an external-repo fact I can't see from this checkout. If nothing renders it → retire outright. If something does → re-home as an opt-in "thread digest" read with deterministic membership (rows derived only from delivered events, no cache-luck)

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:24:28.452Z*

---
