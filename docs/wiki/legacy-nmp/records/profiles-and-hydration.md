---
title: "Profiles and Hydration"
source: "legacy-NMP forensic recovery"
record_count: 11
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Profiles and Hydration

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-PROFILES-2171D4152E (explanation)

> 1. when a user signs up, a kind:0 is supposed to be published
> 2. yet when the user signs up the and taps on their own profile in the top-left button, there is nothing but a placeholder
> 3. also, their own profile view shows a "follow" button, it should be an "edit" button to edit their profile

*Provider: codex | Session: 019e444a-e301-7f13-b133-efc4eea66155 | Timestamp: 2026-05-20T07:34:39.615Z*

---

### LNK-PROFILES-8C4ED4A575 (rationale)

> 1. the user logs in
> 2. nmp triggers the REQ of the users 10002
> 3. chirp ios or any app wants to show "a list of events from the user's follows" -> the follows list is retrieved -> since we then send a REQ for a filter that has authors: <all-follows>
> 4. <all-follows> 10002 event is retrieved
> 5. because we retrieve kind:1s or whatever we want to show those users' kind:0 (because we register an interest in that user's kind:0) -> the user's relays should be sent a req for the user's kind:0

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:10:03.601Z*

---

### LNK-PROFILES-1426CD2461 (explanation)

> doesn't show the avatar -- wheras the avatar conponent does

*Provider: codex | Session: 019e5f6a-acb0-7d73-bf40-1020e459f7d1 | Timestamp: 2026-05-25T21:40:15.710Z*

---

### LNK-PROFILES-35A12B5904 (explanation)

**Formulation 1 — 2026-06-22T08:10:09.380Z**

> the way I understand it apps want a "search" that hit across many domains. I might type "Pablo" and could mean an article or any event (i.e. a nip-50 search for an event saying "Pablo" somewhere), a user with a matching kind:0, a group, a relay I'm connected to (wss://pablo.com)

*Provider: codex | Session: 019eee5b-0a65-71d1-af19-3e37f71fdbf9 | Timestamp: 2026-06-22T08:10:09.380Z*

**Formulation 2 — 2026-06-22T08:10:09.380Z**

> I think we need to rethink from first principles what "search" means and a search could be scoped by a type of target, for example an app might say "give me whatever user matches 'pablo'" or 'give me whatever nip29 group matches 'pablo'" or 'give me any article or kind:1 that matches 'pablo'" (which would trigger a nip-50 search in the relevant relays, in addition to (obviously) checking the cache (not sure how we would do nip-50 text-based search on the cache, that's also something we need to consider probably), or an app might say "give me naddr1....." and no search is required, but we desintermediate apps having to realize "oh, the user typed a bech32, we know exactly what we want, we need to find the event behind this naddr1..." or the user might type "pablo@f7z.io" and we can go stragiht to that nip05-resolved user.

*Provider: codex | Session: 019eee5b-0a65-71d1-af19-3e37f71fdbf9 | Timestamp: 2026-06-22T08:10:09.380Z*

---

### LNK-PROFILES-A2C74A9AD4 (rationale)

> Log when the rendering happens so that we know the rendering first rendered the npub (because it didn't have the kind:0 yet) and, once the kind:0s arrived, it re-rendered with the actual names instead

*Provider: codex | Session: 019e6335-8d18-7bb2-bc1e-041204f76881 | Timestamp: 2026-05-26T08:34:21.665Z*

---

### LNK-PROFILES-DF87CB8FBA (rationale)

> but the idea is that we are rendering a bunch of kind:1 constantly from new people (firehose), the idea would be that, because a new avatar or user comes into display, that would end up triggering retrieving a new kind:0 for this pubkey, all you did here is just add another kind:0 REQ, but, if this were working properly, as new notes show up, the kind:0 of those pubkeys might end up getting REQed (once we adopt how NDK works it would aggregate subscriptions with similar signatures at 100ms clips to avoid creating a single kinds:[0], authors:[pubkey] for each pubkey -- do you understand?

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T21:53:46.038Z*

---

### LNK-PROFILES-120CF07DF1 (explanation)

> doesn't show the avatar -- wheras the avatar conponent does

*Provider: codex | Session: 019e5f6a-acb0-7d73-bf40-1020e459f7d1 | Timestamp: 2026-05-25T21:40:15.714Z*

---

### LNK-PROFILES-4F39F4D3DD (explanation)

> not a single note shows actual kind:0 stuff, they all show npub1... they all show "Waiting for selected author kind:0" --- I bet most if not all of them have kind:0s...

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T22:03:35.776Z*

---

### LNK-PROFILES-CC4998027E (explanation)

> on chirp, put a top-left button with the current user's avatar -- tapping it opens the user's profile

*Provider: codex | Session: 019e442b-27ad-74a2-8b30-999ffe498a75 | Timestamp: 2026-05-20T07:21:43.861Z*

---

### LNK-PROFILES-CA204EAE65 (rejection)

> isn't that a concern of the UI? and more broadly, isn't that a concern that is shared by everything and not scoped only to relation counts? for example, if we're showing a list of items in a feed that has 500 items we are supposed to (per nmp-feed?) to already virtualize the list so we don't attempt to show 500 items, with loading of kind:0 of all the authors in the list, etc and instead we only show the stuff that is or might soon be within the screen? This visible-note-relations-action seems like a too-tightly-scoped primitive that must (and already is?) generalized further

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T07:10:49.771Z*

---

### LNK-PROFILES-B965F2F447 (rejection)

> no, it needs to be very clearly stated somehwere in the product spec of the registry

*Provider: codex | Session: 019e6e20-7b14-7d23-a2d6-b0c711d8e19f | Timestamp: 2026-05-28T10:31:32.343Z*

---
