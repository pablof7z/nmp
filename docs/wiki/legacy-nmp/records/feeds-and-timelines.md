---
title: "Feeds and Timelines"
source: "legacy-NMP forensic recovery"
record_count: 49
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Feeds and Timelines

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-FEEDS-CA6798F25B (explanation)

> "NIP-01/NIP-18/NIP-16 adapter decides it also needs repost wrappers 6/16, root hydration, reply attribution, profiles, missing-event claims."

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T21:14:04.835Z*

---

### LNK-FEEDS-4A35689CEF (rationale)

> "Which feed shapes are first-class:" => not sure, but if one is first-class citizen over the others is because we are not at the right level of abstraction -- it should be indiferent to the work we're doing the relay set, whether it's a tag/search, or how it's being displayed on the UI.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T21:40:24.849Z*

---

### LNK-FEEDS-FF636CFF2C (explanation)

> "visible" implies that the crate understands when its being called (e.g. "this thing became visible")
> "note" implies this is about kind:1 notes... not "events"

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T07:17:26.241Z*

---

### LNK-FEEDS-824614724E (rationale)

> * how do we apply wot filtering/sorting?
> * how do we apply any custom filtering/sorting?
> * how do the app knows it's rendering an event not because it matches some requirement (i.e. the user follows the author and we are getting a feed of events the currrent user follows) but because someone the user follows REPOSTED (kind 6 or 16) the event.
> * how do we paginate (i.e. "give me more", i.e. the recent pull-model we implemented yesterday) so we don't retrieve thousands of events only to render 10 in a screen but retrieve and paginate?

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T21:25:50.828Z*

---

### LNK-FEEDS-1D78C8BE40 (explanation)

**Formulation 1 — 2026-07-02T13:47:53.983Z**

> * ## Status
> * “Accepted”
> * “Current”
> * “Amended”
> * “Folded”
> * “Retired”
> * “Supersession note”
> * “Historical note”
> * “Landed state”
> * long PR chronology
> * long issue chronology
> * detailed migration sequence unless it is still the active rollout rule
> * broad API reference tables
> * copied doctrine repeated from another current owner
> * references to deleted files as current authority

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

**Formulation 2 — 2026-07-02T13:47:53.983Z**

> If the historical detail matters, link the issue/PR in Related; do not narrate it in the ADR.

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

**Formulation 3 — 2026-07-02T13:47:53.983Z**

> * A remaining ADR is mostly historical explanation.

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

---

### LNK-FEEDS-DE938300FD (explanation)

> * ## Status
> * “Accepted”
> * “Current”
> * “Amended”
> * “Folded”
> * “Retired”
> * “Supersession note”
> * “Historical note”
> * “Landed state”
> * long PR chronology
> * long issue chronology
> * detailed migration sequence unless it is still the active rollout rule
> * broad API reference tables
> * copied doctrine repeated from another current owner
> * references to deleted files as current authority

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

---

### LNK-FEEDS-649B6FE2FD (rationale)

> For replies we would need to ask for kinds:[1] when the event in question is a kind:1, otherwise we would need to ask for kinds:[1111] per NIP-22.

*Provider: claude | Session: 4d334ed0-c4a2-4054-8bbf-402ac7fe2462 | Timestamp: 2026-06-29T21:06:59.220Z*

---

### LNK-FEEDS-5CEC75458B (explanation)

**Formulation 1 — 2026-06-23T08:25:43.349Z**

> 1406 LOC — docs/perf/pending-user-decisions.md (baseline 1406)

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

**Formulation 2 — 2026-06-23T08:25:43.349Z**

> 13.

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

**Formulation 3 — 2026-06-23T08:25:43.349Z**

> 1040 LOC — docs/perf/op-centric-feed-architecture.md (baseline 1237)

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

**Formulation 4 — 2026-06-23T08:25:43.349Z**

> 6. **For the 2 .md docs** (pending-user-decisions.md, op-centric-feed-architecture.md): these are documentation, not code. Recommend how to split them by topic into separate docs under the same directory, or whether content should move to a durable doc (ADR, architecture doc, builder-guide page) per AGENTS.md's planning discipline.

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

---

### LNK-FEEDS-30C6C45ECC (explanation)

> 1. Add register_typed_snapshot_projection to NmpApp in nmp-ffi
>   2. Wire the typed nmp.feed.home closure in nmp-app-chirp
>   3. Mark nmp_app_chirp_snapshot as #[deprecated] diagnostics-only

*Provider: codex | Session: 019e6dc7-940d-7031-bb54-0505f9bf93b8 | Timestamp: 2026-05-28T10:19:51.571Z*

---

### LNK-FEEDS-31E87D920E (explanation)

> 3. Decision-bearing — record a durable architectural choice, not implementation diary, API reference, migration notes, or historical rationale.

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

---

### LNK-FEEDS-26FBEAF6DA (explanation)

> > " nmp.feed.home — aggregates across a dynamic follow list across multiple relays with ranking. Not a simple filter."

*Provider: claude | Session: a94ce715-1970-433d-bea3-7702b717dc33 | Timestamp: 2026-06-28T09:41:29.293Z*

---

### LNK-FEEDS-F7A51EBB83 (rationale)

**Formulation 1 — 2026-06-19T21:40:24.849Z**

> "What is canonical feed item identity" => I think it should be the target event id.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T21:40:24.849Z*

**Formulation 2 — 2026-06-19T21:40:24.849Z**

> For example, if I follow pubkey 1 and pubkey 2, pubkey1 posts event1, pubkey2 later reposts event1 => that's the same event in my feed, but it should be sorted (if sorting chronologically) higher because pubkey2's repost makes the created_at of the event1 in the feed by sorted by pubkey2's repost created_at (obviously when we render it in the feed we'd show the event1's created_at -- only the sorting chrnological position takes into account the source/wrapper event (the event that makes the event1 be in that position in the feed).

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T21:40:24.849Z*

---

### LNK-FEEDS-64E0039EC6 (rationale)

> I also want you to include an agent that will go over all the decisions and ADRs and review them for consistency -- same thing as well -- whatever is talking about something that has been "superseeded" or that its no longer true because it was removed, we need to DELETE the text from those files instead of leaving notes about something that is NOT true -- leave what it's a design, not what is NOT a design.

*Provider: codex | Session: 019f02a1-9560-7bb3-be86-4fc678343812 | Timestamp: 2026-06-26T06:37:09.082Z*

---

### LNK-FEEDS-C2519BF41A (explanation)

> Concretely:
> - remove RootIndexed as a blessed feed shape;
> - remove Nip10ReplyAttribution from the feed row model;
> - do not replace them with "generic RootIndexed";
> - expose row knobs: admission, canonical row id, source id, sort key, merge policy, typed context, optional render_target_ref;
> - default canonical row id to event.id;
> - let repost-aware feeds set canonical row id to target id;
> - let reply-digest views, if any, live as app/concept-owned grouping built from delivered events;
> - feed may declare refs, but must not call resolve_ref internally.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-F9475F4124 (explanation)

> My current recommendation: make #3082 less about renaming NoteFeedItem and more about deleting the baked timeline shape. The issue already says feed rows should be kind-agnostic and render policy should stay out of reusable crates. The stronger architectural target is: NMP/feed should provide composable feed mechanics; apps build root-indexed/reply-attribution behavior from those mechanics when they want it.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-5AF2E417A1 (explanation)

> The problem is not only that they are kind:1/NIP-10-specific. The deeper problem is that they encode one render model inside feed mechanics: suppress the matched reply, surface the root, and attach a "who replied" attribution list. That is app/concept policy, not generic feed behavior.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-67756DD5EE (explanation)

> The problem is not only that they are kind:1/NIP-10-specific. The deeper problem is that they encode one render model inside feed mechanics: suppress the matched reply, surface the root, and attach a "who replied" attribution list. That is app/concept policy, not generic feed behavior.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-2571CD1391 (explanation)

> The problem is not only that they are kind:1/NIP-10-specific. The deeper problem is that they encode one render model inside feed mechanics: suppress the matched reply, surface the root, and attach a "who replied" attribution list. That is app/concept policy, not generic feed behavior.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-42208DCC6D (rationale)

**Formulation 1 — 2026-07-11T10:47:59.012Z**

> if an app is showing a kinds:[9999], authors:[$currentPubkey] -- for the feed of the current user

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 2 — 2026-07-11T10:47:59.012Z**

> and the app changes

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 3 — 2026-07-11T10:47:59.012Z**

> The reason I push-back is that, imagine if I want to make an app with multi-account and I want to keep a filter for kinds:[9999],"#p":[<all-accounts-in-this-app>] and I could show a little indicator "oh, your account X was p-tagged"

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

---

### LNK-FEEDS-5B639F5987 (requirement)

**Formulation 1 — 2026-07-11T10:47:59.012Z**

> if an app is showing a kinds:[9999], authors:[$currentPubkey] -- for the feed of the current user

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 2 — 2026-07-11T10:47:59.012Z**

> and the app changes

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 3 — 2026-07-11T10:47:59.012Z**

> The reason I push-back is that, imagine if I want to make an app with multi-account and I want to keep a filter for kinds:[9999],"#p":[<all-accounts-in-this-app>] and I could show a little indicator "oh, your account X was p-tagged"

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

---

### LNK-FEEDS-29C81FF50B (decision)

> write a github issue to track this work, capturing all requirements I explicitly stated (so you never rationalize yourself out of something I specifically said) and your research+design thus far. We will keep iterating because I have more notes and questions -- while you publish that, also send an agent to research what's the role of nmp-feed in all this.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T20:43:40.001Z*

---

### LNK-FEEDS-A1202BD548 (rationale)

> write a github issue to track this work, capturing all requirements I explicitly stated (so you never rationalize yourself out of something I specifically said) and your research+design thus far. We will keep iterating because I have more notes and questions -- while you publish that, also send an agent to research what's the role of nmp-feed in all this.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T20:43:40.001Z*

---

### LNK-FEEDS-6D36E87366 (explanation)

> after many seconds it started responding again -- it keeps reordering the feed, showing me at the very top really old events

*Provider: codex | Session: 019e6343-0242-78a3-9d2c-3be94630ecf0 | Timestamp: 2026-05-26T07:51:12.336Z*

---

### LNK-FEEDS-F97D1A90F9 (rationale)

**Formulation 1 — 2026-07-08T14:01:22.344Z**

> and to be honest, in a way, nip-18 reposts are kinda the same; we cought an event that matched what we asked for ("kind 9999 published by an author in `authors` or whatever filter it is) and, instead of showing the repost (kind 6 or 16, whatever) we actually show a *different* event (what was actually rendered) -- so in a way this is two instances of the same type of indirection; and perhaps none of them should actually live on NMP but apps should have a very simple of way of saying "ah, I want to render event X, whether that is because we got it from nmp-feed or because we got a *pointer* that we want to translate to some otehr event.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:01:22.344Z*

**Formulation 2 — 2026-07-08T14:01:22.344Z**

> It really is the same nature, isn't it?

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:01:22.344Z*

---

### LNK-FEEDS-803569484E (rationale)

> because, let me tell you, the fact that we have this "resolver" thing is a total smell -- I suspect we are doing something in NMP that should be done by an app's logic; i.e. maybe nmp-feed returns "this is the event that was retrieved for the feed based on what you wanted" and if an app, instead of showing that thing wants to show an indirection to some different event with some badge (i.e. the root event with a "so and so replied to this") then that sounds like an indirection/rewrite of *what to render* that an app would own

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T13:58:25.281Z*

---

### LNK-FEEDS-CFAED8C50B (rationale)

> dude, just because a decision is documented doesn't mean is RIGHT! the earth was documented as fucking flat... does that make it fucking right?

*Provider: claude | Session: 89c4af26-df83-4700-94cb-76cd12614c62 | Timestamp: 2026-06-28T10:20:46.867Z*

---

### LNK-FEEDS-D217CE37A3 (explanation)

> for example, an app might say "the user wants to reply to event x"

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T11:13:28.342Z*

---

### LNK-FEEDS-3A3AEBE4FB (requirement)

> git pull and fully implement, for real now (we are moving towards implementation now) per
>   the plan in the commit -- use my npub as the slice test pubkey
>   npub1l2vyh47mk2p0qlsku7hg0vn29faehy9hy34ygaclpn66ukqp3afqutajft and use the suggested seed
>   dev accounts for nmp-demo timeline test

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T20:08:07.651Z*

---

### LNK-FEEDS-47E2286F9E (rationale)

> home feed is probably the worst example: the home feed must have a very specific subscription semantic, like all kinds:[x], authors: [<people-the-user-follows>] (where x is whatever is the feed is about, could be kinds:1, could be kinds:30023, could be whatever.

*Provider: codex | Session: 019f005e-9925-7d42-a496-2c89e1fa2a96 | Timestamp: 2026-06-25T20:21:10.997Z*

---

### LNK-FEEDS-5F106CE424 (explanation)

> if $currentPubkey is truly reactive the exact same feed will immedaitely rerun to show the correct feed with the now-logged-in-pubkey.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

---

### LNK-FEEDS-B1C6EE5460 (explanation)

**Formulation 1 — 2026-07-11T10:47:59.012Z**

> if an app is showing a kinds:[9999], authors:[$currentPubkey] -- for the feed of the current user

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 2 — 2026-07-11T10:47:59.012Z**

> and the app changes

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

**Formulation 3 — 2026-07-11T10:47:59.012Z**

> The reason I push-back is that, imagine if I want to make an app with multi-account and I want to keep a filter for kinds:[9999],"#p":[<all-accounts-in-this-app>] and I could show a little indicator "oh, your account X was p-tagged"

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:47:59.012Z*

---

### LNK-FEEDS-2B62885C3F (explanation)

**Formulation 1 — 2026-06-19T21:29:24.048Z**

> one thing I will want at some point and that might be useful to consider now because it might provide the right level of abstraction -- what if I want to say "give me a feed from people in this kind 1xxxx feed" -- it's essentially the same as saying "give me a feed of people the user X follows" (we grab the user's kind:3, get all the pubkeys and retrieve events with authors:[<all-those-pubkeys>]

*Provider: codex | Session: 019ee1c6-8e45-7df0-9915-3a08d2f16fdb | Timestamp: 2026-06-19T21:29:24.048Z*

**Formulation 2 — 2026-06-19T21:29:24.048Z**

> in the case of "give me a feed of people in this kind 1xxxx feed" it's literally the same thing: retrieve the kind 1xxxx event in reference (for example I might say "give me all people fiatjaf muted in his 10000 list) and construct a kinds:[X], authors:[<people-in-that-10000-event>]

*Provider: codex | Session: 019ee1c6-8e45-7df0-9915-3a08d2f16fdb | Timestamp: 2026-06-19T21:29:24.048Z*

---

### LNK-FEEDS-F0266E8D45 (explanation)

> key = "nmp.feed.home"
> schema_id = "nmp.nip01.timeline" sounds good

*Provider: codex | Session: 019e6dc7-940d-7031-bb54-0505f9bf93b8 | Timestamp: 2026-05-28T10:49:23.903Z*

---

### LNK-FEEDS-C09B4E2547 (explanation)

> - let reply-digest views, if any, live as app/concept-owned grouping built from delivered events;

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-2C1DEB5BFF (explanation)

> nmp.feed.home shouldn't exist! that was at the core of what I said initially when we started working on this file that specifically as a gh issue -- anything like that is a total smell!

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T12:21:12.758Z*

---

### LNK-FEEDS-58395B5E58 (rationale)

**Formulation 1 — 2026-06-19T21:29:24.048Z**

> one thing I will want at some point and that might be useful to consider now because it might provide the right level of abstraction -- what if I want to say "give me a feed from people in this kind 1xxxx feed" -- it's essentially the same as saying "give me a feed of people the user X follows" (we grab the user's kind:3, get all the pubkeys and retrieve events with authors:[<all-those-pubkeys>]

*Provider: codex | Session: 019ee1c6-8e45-7df0-9915-3a08d2f16fdb | Timestamp: 2026-06-19T21:29:24.048Z*

**Formulation 2 — 2026-06-19T21:29:24.048Z**

> in the case of "give me a feed of people in this kind 1xxxx feed" it's literally the same thing: retrieve the kind 1xxxx event in reference (for example I might say "give me all people fiatjaf muted in his 10000 list) and construct a kinds:[X], authors:[<people-in-that-10000-event>]

*Provider: codex | Session: 019ee1c6-8e45-7df0-9915-3a08d2f16fdb | Timestamp: 2026-06-19T21:29:24.048Z*

**Formulation 3 — 2026-06-19T21:29:24.048Z**

> what would be a more abstract design that actually captures the expressivity of this?

*Provider: codex | Session: 019ee1c6-8e45-7df0-9915-3a08d2f16fdb | Timestamp: 2026-06-19T21:29:24.048Z*

---

### LNK-FEEDS-4DE248AA4E (explanation)

> - nmp-feed owns reliable mechanics: admission, canonical row identity, dedupe, ordering, paging/windowing, lifecycle, source tracking, and merge/recompute.
> - protocol crates own protocol facts: NIP-10 root/reply parsing, NIP-18 target extraction, NIP-29 group identity, etc.
> - apps/concepts own render policy: whether to render the matched event, a referenced event, a root, a badge, a collapsed group, or a plain row.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-08T14:47:22.517Z*

---

### LNK-FEEDS-AE39195F74 (explanation)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> we also need to have an app relay concept which is a relay that always gets all queries regardless of outbox planning (i.e. a user might have relay1 on their kind 10002) but when we REQ we would still route the REQ to the app relay (which is app-supplied)

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> say later the app wants to show a feed of their kinds:[30023]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-FEEDS-518E93DF99 (explanation)

> show me some small portions of code detailing what an app does to setup their home feed, for example if an app wants to say "a feed of kinds 30023 of the people the user follows or of 30023 events the user's follows have reacted to or commented on"

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T20:56:12.806Z*

---

### LNK-FEEDS-07D4A670F6 (rationale)

> there are notes in docs/ that include old/retired/legacy stuff -- we should REMOVE stuff that's no longer true -- it's fine to leave a single one-liner note explaining that something no longer exists for reference so that we don't accidentally reintroduce it (other docs/wiki/* -- those are ok -- dont even read them)

*Provider: codex | Session: 019f02a1-9560-7bb3-be86-4fc678343812 | Timestamp: 2026-06-26T06:35:20.878Z*

---

### LNK-FEEDS-61D69241FE (explanation)

> “my follows’ notes” is not app code watching kind:3 and reopening REQs. It is a data declaration:

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-FEEDS-37484975F1 (correction)

> there are notes in docs/ that include old/retired/legacy stuff -- we should REMOVE stuff that's no longer true -- it's fine to leave a single one-liner note explaining that something no longer exists for reference so that we don't accidentally reintroduce it (other docs/wiki/* -- those are ok -- dont even read them)

*Provider: codex | Session: 019f02a1-9560-7bb3-be86-4fc678343812 | Timestamp: 2026-06-26T06:35:20.878Z*

---

### LNK-FEEDS-DED0259946 (rejection)

> Do not edit production code.

*Provider: claude | Session: a3701802-612f-422f-9a08-37ec9debd4c9 | Timestamp: 2026-06-20T21:18:50.427Z*

---

### LNK-FEEDS-8F6AFBF338 (rejection)

> Do not write this ADR as a feed-specific exception to ADR-0070.

*Provider: codex | Session: 019f1f84-aaa9-71c2-b012-48812bfbf20d | Timestamp: 2026-07-01T21:19:26.484Z*

---

### LNK-FEEDS-31CEBB371A (rejection)

> no legacy! NEVER ANY LEGACY!!!

*Provider: codex | Session: 019f177c-e16c-7f13-9deb-a9206ab23bcb | Timestamp: 2026-06-30T07:53:45.463Z*

---

### LNK-FEEDS-3D9818CA76 (rejection)

> I also want you to include an agent that will go over all the decisions and ADRs and review them for consistency -- same thing as well -- whatever is talking about something that has been "superseeded" or that its no longer true because it was removed, we need to DELETE the text from those files instead of leaving notes about something that is NOT true -- leave what it's a design, not what is NOT a design.

*Provider: codex | Session: 019f02a1-9560-7bb3-be86-4fc678343812 | Timestamp: 2026-06-26T06:37:09.082Z*

---

### LNK-FEEDS-9D17C99EEB (rejection)

> that's fucking retarded! NO! nmp.nip29 MUST have a SINGLE function to publish ANY event! it doesn't know ANYTHING about how other kinds' replies are published, it doesn't know ANYTHING about concerns of other crates -- all nip29 crate understands is concerns of nip29 -- group creation, group mgmt, group discovery, and all group* things, it doesn't know "generate a reaction to event x" or "publish a reply to event y"

*Provider: claude | Session: d9be7578-af06-4047-a00f-92ab4fa1189d | Timestamp: 2026-06-28T08:14:56.895Z*

---

### LNK-FEEDS-F2E6EC7282 (rejection)

> no legacy! NEVER ANY LEGACY!!!

*Provider: codex | Session: 019f177c-e16c-7f13-9deb-a9206ab23bcb | Timestamp: 2026-06-30T07:53:45.463Z*

---

### LNK-FEEDS-B44C1AC46B (rejection)

> to be clear; I think we shouldn't request the app to give us/track a signer all the time -- 99% of the cases currentPubkey is the signer we want to sign with -- we just need a way to override that

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:56:17.082Z*

---
