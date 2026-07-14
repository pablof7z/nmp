---
title: "Reactivity, Views, and Projections"
source: "legacy-NMP forensic recovery"
record_count: 16
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Reactivity, Views, and Projections

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-EVENTDRIVEN-985748BBD0 (explanation)

> It had real valuable machinery: outbox routing, relay diagnostics, projection revisions, store semantics, transport lessons, no-polling
>      doctrine, Rust-owned domain logic.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-UPDATES-DBA3E05FAA (rationale)

**Formulation 1 — 2026-07-11T10:07:54.794Z**

> That is an unknowable proposition, and this is practically very true -- many relays go offline, are slow, so "syncHealth" makes apps think that they will at some point receive some "complete" view, which is impossible to know because you don't know if I'm running a relay in my LAN that also matches the user's filter.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:07:54.794Z*

**Formulation 2 — 2026-07-11T10:08:42.146Z**

> That is an unknowable proposition, and this is practically very true -- many relays go offline, are slow, so "syncHealth" makes apps think that they will at some point receive some "complete" view, which is impossible to know because you don't know if I'm running a relay in my LAN that also matches the user's filter.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T10:08:42.146Z*

---

### LNK-UPDATES-F39487A1C9 (explanation)

> - The Swift-side FlatBuffers Verifier runs on every decode, but the snapshot comes from our own in-process Rust (trusted) — that verification is arguably skippable on this path.

*Provider: claude | Session: 78c8ec3a-f558-4738-98af-1f3af4978ec4 | Timestamp: 2026-06-13T18:45:36.660Z*

---

### LNK-UPDATES-D5ADE0AB1C (explanation)

> tell me more about the 4hz snapshot

*Provider: claude | Session: da6b1d73-e1c8-4765-8ac7-056aa90fc154 | Timestamp: 2026-06-11T10:20:29.718Z*

---

### LNK-VIEWS-5DB26479FB (rationale)

> Don't just not consider deleting something because its used by something else -- review whether it *should* be being used or if its need has been surperseeded by something better

*Provider: codex | Session: 019f221e-d9d2-7473-a1a2-8eafbea46fae | Timestamp: 2026-07-02T09:39:18.772Z*

---

### LNK-VIEWS-627A8360A3 (requirement)

> commit within this same draft PR the proposed reactivity framework you're proposing

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T08:49:38.814Z*

---

### LNK-VIEWS-9A83A4C191 (rationale)

> don't take the existing ADRs as settled word -- the exercise we're embarking on is precisely re-arcchitecture -- so just because a document says that something should work in one way doesn't mean it shouldn't be changed

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T07:52:50.147Z*

---

### LNK-VIEWS-0B86EEC142 (explanation)

> sounds like projection.nmp.nip29.group_defaults shouldn't exist then

*Provider: claude | Session: 2093d19c-ec37-482d-8cb4-f061aa0e3ef0 | Timestamp: 2026-07-01T09:04:24.889Z*

---

### LNK-VIEWS-E0C58BD123 (explanation)

**Formulation 1 — 2026-07-01T13:21:20.939Z**

> we're working on reactivity stuff -- look at github issues and prs from today. In that context:

*Provider: codex | Session: 019f1dd5-59c1-76a2-9d81-de6525c5be49 | Timestamp: 2026-07-01T13:21:20.939Z*

**Formulation 2 — 2026-07-01T13:21:20.939Z**

> what would the code for an app look like to do that once all the reactivity stuff we're working on lands?

*Provider: codex | Session: 019f1dd5-59c1-76a2-9d81-de6525c5be49 | Timestamp: 2026-07-01T13:21:20.939Z*

---

### LNK-VIEWS-5337DC6957 (rationale)

> where are we in terms of migrations/work in order to make ../29er be able to benefit from this? -- also mention in $tenex-edge your intention of making 29er benefit from this reactivity stuff (I don't know if its already planned)

*Provider: codex | Session: 019f1dd5-59c1-76a2-9d81-de6525c5be49 | Timestamp: 2026-07-01T13:50:44.199Z*

---

### LNK-VIEWS-FA86BB5B57 (explanation)

**Formulation 1 — 2026-07-01T08:34:25.096Z**

> ● namespace projection.nmp.nip51.mute_list  [EXCLUSIVE]
>       projection = nmp.nip51.mute_list
>       · mute-list projection key

*Provider: claude | Session: 2093d19c-ec37-482d-8cb4-f061aa0e3ef0 | Timestamp: 2026-07-01T08:34:25.096Z*

**Formulation 2 — 2026-07-01T08:34:25.096Z**

> ● namespace projection.nmp.nip51.bookmarks  [EXCLUSIVE]
>       projection = nmp.nip51.bookmarks
>       · bookmarks projection key

*Provider: claude | Session: 2093d19c-ec37-482d-8cb4-f061aa0e3ef0 | Timestamp: 2026-07-01T08:34:25.096Z*

---

### LNK-VIEWS-C4C5382824 (explanation)

**Formulation 1 — 2026-07-01T08:34:25.096Z**

> ● namespace projection.nmp.nip51.mute_list  [EXCLUSIVE]
>       projection = nmp.nip51.mute_list
>       · mute-list projection key

*Provider: claude | Session: 2093d19c-ec37-482d-8cb4-f061aa0e3ef0 | Timestamp: 2026-07-01T08:34:25.096Z*

**Formulation 2 — 2026-07-01T08:34:25.096Z**

> ● namespace projection.nmp.nip51.bookmarks  [EXCLUSIVE]
>       projection = nmp.nip51.bookmarks
>       · bookmarks projection key

*Provider: claude | Session: 2093d19c-ec37-482d-8cb4-f061aa0e3ef0 | Timestamp: 2026-07-01T08:34:25.096Z*

---

### LNK-EVENTDRIVEN-4F51B404DA (rejection)

> swift polls for data in the app? why? I dont want polling! is that in how this thing is designed to work? I HATE polling

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T21:36:50.572Z*

---

### LNK-EVENTDRIVEN-02CF88FEA9 (rejection)

> its very obviously bad design -- so whether we keep register_snapshot_tick_observer or remove it entirely -- this polling for "who's the current user" is wrong and must be rearchitected -- get codex exec to rearchitect it -- don't mention any solution, only mention the problem (i.e. never prime codex exec with how you would solve it)

*Provider: claude | Session: ee4b1b7e-f139-4488-85f0-dbf1dda40f1a | Timestamp: 2026-06-28T08:33:49.055Z*

---

### LNK-VIEWS-A876F9FF7A (rejection)

> Don't just not consider deleting something because its used by something else -- review whether it *should* be being used or if its need has been surperseeded by something better

*Provider: codex | Session: 019f221e-d9d2-7473-a1a2-8eafbea46fae | Timestamp: 2026-07-02T09:39:18.772Z*

---

### LNK-VIEWS-A53A70158E (rejection)

> don't take the existing ADRs as settled word -- the exercise we're embarking on is precisely re-arcchitecture -- so just because a document says that something should work in one way doesn't mean it shouldn't be changed

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T07:52:50.147Z*

---
