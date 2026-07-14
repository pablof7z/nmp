---
title: "Outbox, Networking, and Relay Roles"
source: "legacy-NMP forensic recovery"
record_count: 21
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Outbox, Networking, and Relay Roles

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-OUTBOX-8E163732E0 (rationale)

> * proves minimum 2-relay intersection per author or p-tag pubkey to probe we don’t connect to hundreds of relays just because a few people have dozens of relays.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T21:10:35.902Z*

---

### LNK-OUTBOX-AD798FC925 (explanation)

> // each author gets an attempted coverage of two relays per what their kind:10002 announces

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-BC83586899 (explanation)

> 1. the user logs in
> 2. nmp triggers the REQ of the users 10002
> 3. chirp ios or any app wants to show "a list of events from the user's follows" -> the follows list is retrieved -> since we then send a REQ for a filter that has authors: <all-follows>
> 4. <all-follows> 10002 event is retrieved
> 5. because we retrieve kind:1s or whatever we want to show those users' kind:0 (because we register an interest in that user's kind:0) -> the user's relays should be sent a req for the user's kind:0

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:10:03.601Z*

---

### LNK-OUTBOX-27CF38F913 (rationale)

**Formulation 1 — 2026-05-20T06:53:47.804Z**

> since it autofollows those two accounts and the timeline its supposed to be set up to REQ from people the user follows, withour chirp doing anything (the rust kernel should be
>   doing this kind of thing automatically) it retrieves the relays of those two pubkeys, and keeps a REQ open in their respective relays (because of outbox) to retrieve their posts, as
>   well as the posts of the logged in user.

*Provider: codex | Session: 019e4429-0ea8-7490-8357-3751f83ebfd6 | Timestamp: 2026-05-20T06:53:47.804Z*

**Formulation 2 — 2026-05-20T06:53:47.804Z**

> publish a post from the account; it should show up on the timeline.

*Provider: codex | Session: 019e4429-0ea8-7490-8357-3751f83ebfd6 | Timestamp: 2026-05-20T06:53:47.804Z*

---

### LNK-OUTBOX-93ABE709F4 (explanation)

> 1. the user logs in
> 2. nmp triggers the REQ of the users 10002
> 3. chirp ios or any app wants to show "a list of events from the user's follows" -> the follows list is retrieved -> since we then send a REQ for a filter that has authors: <all-follows>
> 4. <all-follows> 10002 event is retrieved
> 5. because we retrieve kind:1s or whatever we want to show those users' kind:0 (because we register an interest in that user's kind:0) -> the user's relays should be sent a req for the user's kind:0

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:10:03.601Z*

---

### LNK-OUTBOX-8170817BB7 (rationale)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> the app developer sets indexerRelay:[relay0] and appRelay:[my-app-relay]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> REQs for kind:0,3,10002 of all three users would go to: relay0 AND my-app-relay -- AND, if we happen to already have the 10002 of some of those users (because we had queried them in the past already) we also query their relays -- say we have the kind:10002 of user1 only already cached -- if the app asks for kind:0 of all three users it would do:

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 3 — 2026-07-11T07:12:32.806Z**

> say later the app wants to show a feed of their kinds:[30023]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 4 — 2026-07-11T07:12:32.806Z**

> For example say we have user 4 that doesn't have a 10002 and we don't have in the app an appRelay configured.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 5 — 2026-07-11T07:12:32.806Z**

> REQs of the kinds 10002 would go to r0
> REQs for content, since we don't have a relay for the u4 go to r1.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-6EC7D1416C (rationale)

> I’m sure I didn’t mention things that I must have speced a million times; please send a few agents to review other things I might have mentioned over and over as non-negotiables. So we can nail down this MVP

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T21:10:35.902Z*

---

### LNK-OUTBOX-EEC40186C4 (explanation)

> Log in with an nsec or multiple, see the kinds X (where X is any kind the user of the app can change and the queries change in realtime from observing the input). Two modes: one where the user taps a toggle “my follows” and another where the user selects from a list of not-hardcoded relays (the relays come from the 10002 of the people the user follows) or from bookmarked relay sets (nip-51) sorted by how many people have bookmarked a relay.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T21:10:35.902Z*

---

### LNK-OUTBOX-3AECC5D732 (rationale)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> We open a REQ in their relays:

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> We open this subscription because perhaps the indexer has an old kind:0 or 10002 for them; we receive a newly updated 10002 so we start looking at that one; the app might receive and render the old kind:0 the indexer had and, when/if a new kind:0 comes in it's reactively gets the new kind:0 content.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 3 — 2026-07-11T07:12:32.806Z**

> For example say we have user 4 that doesn't have a 10002 and we don't have in the app an appRelay configured.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-038176DAC5 (rationale)

> Which is a great side-effect: if u4 didn't publish a 10050 event of where they want to receive their DMs, then the nip17 crate would come out with "I don't have a relay where to publish for this user" and would simply refuse to publish because it doesn't have a destination, even if there's an appRelay configured.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:27:11.766Z*

---

### LNK-OUTBOX-85DB13E88D (rationale)

> for example if I randomly type a pubkey to go to a user's profile somewhere, even if I don't follow that user that should trigger a rendering of that user's profile, and because we don't have their kind:0 or anything from them that would end up reactively triggering the REQ for the 10002, the connection to that user's relays, the REQ for that user's kind:0, etc ,etc.

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:11:42.701Z*

---

### LNK-OUTBOX-6E787F60E0 (explanation)

> for example user1's 10002 has [relay1, relay2, relay3, user2 has [relay2, relay4, relay5], user3 has [relay1, relay 4, relay5]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-6CB4869C15 (explanation)

> It had real valuable machinery: outbox routing, relay diagnostics, projection revisions, store semantics, transport lessons, no-polling
>      doctrine, Rust-owned domain logic.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-OUTBOX-93580B5E35 (rationale)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> relay0: REQ kind:[0,10002], authors:[u1,u2,u3]
> my-app-relay: REQ kind:[0,10002], authors:[u1,u2,u3]
> relay1 and relay2: REQ kind:[0,10002], authors:[u1] --- only u1 because is the one we already had their 10002 cached for and we grab max two relays from their kind:10002

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> then we receive the 10002 for the two users we didn't have it cached for (u2 and u3).

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 3 — 2026-07-11T07:12:32.806Z**

> relay2 and relay4: REQ kinds:[0,10002], authors:[u2,u3]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 4 — 2026-07-11T07:12:32.806Z**

> my-app-relay: REQ kinds:[30023], authors:[u1,u2,u3]
> relay1: REQ kinds:[30023], authors:[u1, u3]
> relay2: REQ kinds:[30023], authors:[u1, u2]
> relay4: REQ kinds:[30023], authors:[u2, u3]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-E5D32D7AAA (explanation)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> relay0: REQ kind:[0,10002], authors:[u1,u2,u3]
> my-app-relay: REQ kind:[0,10002], authors:[u1,u2,u3]
> relay1 and relay2: REQ kind:[0,10002], authors:[u1] --- only u1 because is the one we already had their 10002 cached for and we grab max two relays from their kind:10002

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> relay2 and relay4: REQ kinds:[0,10002], authors:[u2,u3]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 3 — 2026-07-11T07:12:32.806Z**

> my-app-relay: REQ kinds:[30023], authors:[u1,u2,u3]
> relay1: REQ kinds:[30023], authors:[u1, u3]
> relay2: REQ kinds:[30023], authors:[u1, u2]
> relay4: REQ kinds:[30023], authors:[u2, u3]

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-7B34055AB1 (rationale)

**Formulation 1 — 2026-05-27T10:12:23.577Z**

> the chirp-tui shows me this:

*Provider: codex | Session: 019e68e9-50d3-79c3-ab93-84cab90a88db | Timestamp: 2026-05-27T10:12:23.577Z*

**Formulation 2 — 2026-05-27T10:12:23.577Z**

> that event count doesn't sound legit -- particularly because purplepag.es is set as an indexer relay on chirp-tui so it should receive all the kind:0,3,10002 just like primal.net does -- plus, if we are connecting to these other relays its because we REQed something from them -- I don't believe for a second they are all coming back empty or that they don't have anything that primal.net doesn't have

*Provider: codex | Session: 019e68e9-50d3-79c3-ab93-84cab90a88db | Timestamp: 2026-05-27T10:12:23.577Z*

---

### LNK-OUTBOX-88E288154D (explanation)

**Formulation 1 — 2026-07-11T07:12:32.806Z**

> REQs for kind:0,3,10002 of all three users would go to: relay0 AND my-app-relay -- AND, if we happen to already have the 10002 of some of those users (because we had queried them in the past already) we also query their relays -- say we have the kind:10002 of user1 only already cached -- if the app asks for kind:0 of all three users it would do:

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 2 — 2026-07-11T07:12:32.806Z**

> relay0: REQ kind:[0,10002], authors:[u1,u2,u3]
> my-app-relay: REQ kind:[0,10002], authors:[u1,u2,u3]
> relay1 and relay2: REQ kind:[0,10002], authors:[u1] --- only u1 because is the one we already had their 10002 cached for and we grab max two relays from their kind:10002

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

**Formulation 3 — 2026-07-11T07:12:32.806Z**

> then we receive the 10002 for the two users we didn't have it cached for (u2 and u3).

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:12:32.806Z*

---

### LNK-OUTBOX-D1E2099C92 (rationale)

> yes, the important part is that event construction is composable: we can use a template event builder (like "react to event x with `+`" or "construct a reply to event y" [ which happens to use nip-22 automatically because its replying to a non kind:1 event ], and the publish action might trigger some more envelope mutations (like an h tag for nip29) or explicitly optout the event publishing from outbox planning (like in the case of nip-17 dms or nip-29 events that each crate has a different rule of which relays need to be published to))

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T11:19:24.837Z*

---

### LNK-RELAYS-401279802B (explanation)

> readEligibleRelayUrls as a thin-shell violation finding. That's a useful partial result: Swift is parsing relay role tokens,
>   which belongs in Rust.

*Provider: codex | Session: 019e4bf6-c04e-7d93-a0bb-4da694c4330f | Timestamp: 2026-05-21T19:15:39.144Z*

---

### LNK-OUTBOX-7481C01424 (rejection)

> no, that's a total hack! this is not a race condition, even if I wait for 10 hours the fucking profile of the just-created user is still not there-- don't fucking hack it, fix it properly!

*Provider: codex | Session: 019e444a-e301-7f13-b133-efc4eea66155 | Timestamp: 2026-05-20T07:52:00.643Z*

---

### LNK-RELAYS-D8BADCA759 (rejection)

**Formulation 1 — 2026-06-14T21:02:35.083Z**

> there are many things wrong with chirp iOS that make it look like a completely naive app (specially the fact that a lot of pubkeys never resolve, I want to get to the bottom of why this pubkeys don't resolve; is it chirp ios fault?

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:02:35.083Z*

**Formulation 2 — 2026-06-14T21:02:35.083Z**

> is it nmp ui ios components' fault?

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:02:35.083Z*

**Formulation 3 — 2026-06-14T21:02:35.083Z**

> is it the nmp kernel fault?

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:02:35.083Z*

**Formulation 4 — 2026-06-14T21:02:35.083Z**

> does chirp ios use app relays?

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T21:02:35.083Z*

---
