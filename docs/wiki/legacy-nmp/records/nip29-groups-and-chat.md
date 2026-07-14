---
title: "NIP-29 Groups and Chat"
source: "legacy-NMP forensic recovery"
record_count: 12
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# NIP-29 Groups and Chat

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-NIP29-2D6E31251A (explanation)

**Formulation 1 — 2026-06-30T11:32:48.805Z**

> - #2525 — layer-inversion doctrine-lint gate.

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

**Formulation 2 — 2026-06-30T11:32:48.805Z**

> Rules A (display-in-primitive), C (nip29 kind-blind), D (nmp-core protocol-noun), E (upward Cargo-edge ban) are good.

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

**Formulation 3 — 2026-06-30T11:32:48.805Z**

> Pablo has been editing this branch directly.

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

---

### LNK-NIP29-A2D1822EA2 (explanation)

**Formulation 1 — 2026-06-28T11:13:28.342Z**

> react_event = react_to_event(event, '+')
> publish_event(react_event)

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T11:13:28.342Z*

**Formulation 2 — 2026-06-28T11:13:28.342Z**

> // or publishing an article in a nip29 group
> article = new_article()
> article.content = 'this is my article'
> article.title = 'Hello World'
> nip29.publish_event_to_group(article, 'group-id') // it adds the 'h' tag, it publishes to the appropriate nip29 relay

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T11:13:28.342Z*

**Formulation 3 — 2026-06-28T11:13:28.342Z**

> // or replying to a nip29 event
> reply_event = reply_to_event(event)
> reply_event.content = 'nice!'
> nip29.publish_event_to_group(reply) // maybe it gets the h tag and relay from the event id we're replying to? or the app maybe just passess it if that's simpler

*Provider: codex | Session: 019f0dc3-5b56-79d1-a14b-5746c93e5879 | Timestamp: 2026-06-28T11:13:28.342Z*

---

### LNK-NIP29-4F0351F670 (explanation)

> Open coordination items:
> 1. nip29 reconciliation: #2540 (nip29_kind_blind lint rule + docs) vs #2542 (guard + seam) vs #2525 Rule C — three overlapping nip29 lint/guard surfaces; dedupe to one. #2542's unique load-bearing value is the public nip25/nip18 builders.
> 2. After #2526 lands, the gate's Rule E must baseline-or-clear the nmp-core→nmp-nostr-id edge (it's downward/legal, so it should just pass).

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

---

### LNK-NIP29-84B9F47DD3 (rationale)

> I actually want an explanation of what sent you down this design path because that means
>   something is seriously wrong on NMP

*Provider: claude | Session: 44730485-3ee0-46c4-b06a-5037afca5b0f | Timestamp: 2026-06-25T20:45:47.761Z*

---

### LNK-NIP29-8F177B1ABB (rationale)

> I mentioned nip29 and nip17 but these are not the only ones; these are just examples of a primitive that NMP needs to support (drafts is another example; in the case of drafts is also interesting because we SHOULD use the appRelay or fallbackRelay if the user didn't publish a "drafts relays")

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-11T07:27:11.766Z*

---

### LNK-NIP29-CDA73C0891 (explanation)

> I said this a fucking million times: there MUSTNT BE ANY FUCKING KIND on the nip29 crate other than SPECIFICALLY the 9xxx and 3900x events nip29 uses.

*Provider: claude | Session: 898a41b5-68e0-4b0f-b16c-c6072454bd6a | Timestamp: 2026-06-30T05:20:12.582Z*

---

### LNK-NIP29-FC7B8D5A36 (explanation)

**Formulation 1 — 2026-07-04T20:37:38.550Z**

> also send an agent to do UX/UI critique at regular intervals to ensure the actual product's UX/UI is good for the different screens -- done tastefully - there are MANY things that I see wrong, done in non-standard liquid glass way, or that feel like the product is off -- at some point send a UX/UI critique agent to audit each screen and each series of screens and flows to ensure the Chirp iOS product is good -- there should be many corrections done with taste and following stnadard ios patterns (for example the diagnostics screen is an utter mess, even getting to it makes no sense, the relay mgmt screen is really bad, the profile screen has a very uncofortable top bar gray area -- the division between Chats / Groups is very awkward -- joining existing NIP

*Provider: claude | Session: dcc80382-bcc0-45ea-8b9c-1a2fc741f872 | Timestamp: 2026-07-04T20:37:38.550Z*

**Formulation 2 — 2026-07-04T20:37:38.550Z**

> NIP-29 groups is literally impossible (there's not even a way to browse NIP-29 groups of different relays)

*Provider: claude | Session: dcc80382-bcc0-45ea-8b9c-1a2fc741f872 | Timestamp: 2026-07-04T20:37:38.550Z*

---

### LNK-NIP29-79E2CE11BE (explanation)

> and nip29 would be in charge of setting the right h-tags for the group and publishing to the right relay

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T09:47:23.569Z*

---

### LNK-NIP29-D7EC85325F (rationale)

> how is all this expected to work from a high level? because this is starting to sound again like bad architecture, exceptions, drifting away from these projections design? -- what is all that?

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-27T05:20:22.771Z*

---

### LNK-NIP29-C6DF0CFA3C (rejection)

> Don’t change anything yet; first let me see if you are capable of generalizing and understanding the rule you violated

*Provider: claude | Session: 898a41b5-68e0-4b0f-b16c-c6072454bd6a | Timestamp: 2026-06-30T06:21:46.741Z*

---

### LNK-NIP29-81717C91BB (rejection)

> Leave a doctrine correction in the guidelines of how this project operates so you never get inclined to break SRP again like this.

*Provider: claude | Session: 898a41b5-68e0-4b0f-b16c-c6072454bd6a | Timestamp: 2026-06-30T06:21:46.741Z*

---

### LNK-NIP29-AE2F014D23 (rejection)

> stop fucking talking about NIP-29! this is not about fucking nip-29!!!! this is about fucking establishing a rule that will CATCH somethjing like this fucking violation! you went completely astray from the fucking task at hand, you fucking moron!

*Provider: codex | Session: 019f1760-47e7-7981-82b1-ae3af811c79f | Timestamp: 2026-06-30T07:28:55.337Z*

---
