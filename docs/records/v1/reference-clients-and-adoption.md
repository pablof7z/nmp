---
title: "Reference Clients and Adoption"
source: "legacy-NMP forensic recovery"
record_count: 38
disposition: "historical-context"
authority: "direct-user-evidence"
---

# Reference Clients and Adoption

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-CHIRP-CAD2F634FE (explanation)

> no, continue draining the queue of issues of NMP -- forget about chirp for now

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T15:36:56.892Z*

---

### LNK-CHIRP-12B51B948A (correction)

> deploy background agents to research if the crates and the chirp apps are following this guidance or if they are accumulating technical debt or building in the wrong direction, etc

*Provider: codex | Session: 019e633d-c100-71b0-a392-2c01c59b529d | Timestamp: 2026-05-26T07:47:01.489Z*

---

### LNK-CHIRP-5B3458ECCD (correction)

> having chirp in this codebase instead of on its own repo has been biting me in the ass too much -- I want to move it to its own repo -- do it -- start by completely removing the code and moving it to ../chirp -- initialize that repo -- work on the nmp repo in a git worktree and PR

*Provider: claude | Session: c7d356fd-1696-4dbb-af3b-2ec1f6ee0ef7 | Timestamp: 2026-06-28T08:22:02.158Z*

---

### LNK-CHIRP-1611CB9EBD (explanation)

> NmpGallery and Chirp are two completely different fucking things!

*Provider: codex | Session: 019e5f6a-acb0-7d73-bf40-1020e459f7d1 | Timestamp: 2026-05-25T20:37:50.940Z*

---

### LNK-CHIRP-2779834623 (explanation)

> apps/chirp shouldn't exist! its been moved to a different repo in ../chirp! DELETE apps/chirp!

*Provider: claude | Session: 3c942260-311d-4e00-8bcc-204045ea87b3 | Timestamp: 2026-06-29T14:33:15.820Z*

---

### LNK-CHIRP-41F6DC5EF8 (requirement)

> deploy background agents to research if the crates and the chirp apps are following this guidance or if they are accumulating technical debt or building in the wrong direction, etc

*Provider: codex | Session: 019e633d-c100-71b0-a392-2c01c59b529d | Timestamp: 2026-05-26T07:47:01.489Z*

---

### LNK-CHIRP-4079204AE3 (explanation)

> either chirp:// is not registered for this app or we 're not passing the callback correctly

*Provider: codex | Session: 019e3c2e-1d92-75c2-a9b9-d9781ffab391 | Timestamp: 2026-05-18T18:26:30.081Z*

---

### LNK-CHIRP-A71E045419 (requirement)

> having chirp in this codebase instead of on its own repo has been biting me in the ass too much -- I want to move it to its own repo -- do it -- start by completely removing the code and moving it to ../chirp -- initialize that repo -- work on the nmp repo in a git worktree and PR

*Provider: claude | Session: c7d356fd-1696-4dbb-af3b-2ec1f6ee0ef7 | Timestamp: 2026-06-28T08:22:02.158Z*

---

### LNK-CHIRP-33AE6C98C9 (explanation)

> some rogue agent accidentally killed a bunch of chirp processes -- was an accident -- relaunch or whatever

*Provider: claude | Session: dcc80382-bcc0-45ea-8b9c-1a2fc741f872 | Timestamp: 2026-07-04T18:03:09.418Z*

---

### LNK-CHIRP-7668DFF188 (requirement)

> the goal of chirp is to be a fully-featured client that demonstrates absolutely everything NMP ships, so the goals in the docs are too unambitious -- fix that and commit

*Provider: codex | Session: 019e442c-cbeb-7e12-9550-3b19f98561c8 | Timestamp: 2026-05-20T07:14:23.507Z*

---

### LNK-CHIRP-FCE5D4CBB0 (explanation)

> the moment primal opens chirp back I'm back to my home screen, if it doesn't crash then its doing something else and stupid

*Provider: codex | Session: 019e3c2e-1d92-75c2-a9b9-d9781ffab391 | Timestamp: 2026-05-18T18:05:00.808Z*

---

### LNK-CHIRP-1D108FBAF2 (rationale)

> to me, right now, what you are doing is as if an operating system expected to implement a printer's printing protocol because it wants to be able to print images or if it had to understand how an SSD is to be used because it wants to write a file

*Provider: codex | Session: 019e6343-0242-78a3-9d2c-3be94630ecf0 | Timestamp: 2026-05-26T10:30:49.031Z*

---

### LNK-CHIRP-51D28654E3 (rationale)

> when an app like chirp renders an event it needs to have some way of indicating "I'm rendering this event, give me a stream of events tagging it" so that an app like chirp can show reactions, responses, reposts, zaps, etc, etc -- do we have something like that already supported somehow?

*Provider: codex | Session: 019e633a-9401-76d0-9a04-442e48c696b1 | Timestamp: 2026-05-26T07:42:59.626Z*

---

### LNK-CHIRP-B4CCC980D6 (rationale)

> you seem to be writing very low-level code to make chirp work... the whole point of chirp is to prove things can be done simple because NMP takes care of all the complex logic... -- you need to identify where that very complex logic belongs or come up with a very sound explanation why it belongs to app-level.

*Provider: codex | Session: 019e6343-0242-78a3-9d2c-3be94630ecf0 | Timestamp: 2026-05-26T10:30:49.031Z*

---

### LNK-CLIENTS-70895E17AB (explanation)

> [tenex-edge] Incoming message mentioning this agent. Treat the following as input addressed to you in this session:

*Provider: claude | Session: 5730fec2-0885-4c09-8606-27f36540aa2c | Timestamp: 2026-06-26T12:02:18.601Z*

---

### LNK-CLIENTS-B044570690 (requirement)

> deploy podcast-player

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-10T09:30:43.539Z*

---

### LNK-CLIENTS-C12A9D76C6 (rationale)

> don't add "superseding ADRs" to correct previous ADRs or any doc -- edit the doc and make it say the right thing -- persist this as a policy of the repo because this has happened many times and it makes no sense to carry forward incorrect data that is corrected in other docs.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T22:28:32.152Z*

---

### LNK-CLIENTS-A1383A1D61 (requirement)

> get to a natural stopping point asap and push -- we'll move this work to a different computer to complete it

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-21T05:02:53.192Z*

---

### LNK-CLIENTS-37278A1641 (explanation)

> highlighter is in ../hl/app

*Provider: codex | Session: 019e37a2-d4e8-7453-9542-b1f97f0a7173 | Timestamp: 2026-05-17T20:41:44.941Z*

---

### LNK-CLIENTS-C9D59BAA87 (explanation)

> it's in the local ~/.tenex-edge

*Provider: claude | Session: 027ae19e-0efb-4c1d-9179-37094a42e9d9 | Timestamp: 2026-06-30T15:55:09.040Z*

---

### LNK-CLIENTS-E36AA45334 (explanation)

> join the tenex-edge #backlog channel and join the pack

*Provider: claude | Session: c58a63aa-a62c-4423-b979-9c7a5758c915 | Timestamp: 2026-06-30T19:41:01.771Z*

---

### LNK-CLIENTS-0B51C1618C (explanation)

> join the tenex-edge #backlog channel, you'll be working on advancing the migration of ../29er

*Provider: claude | Session: 8222a280-2242-44bb-aad9-1bb993d79c7f | Timestamp: 2026-06-30T19:43:11.359Z*

---

### LNK-CLIENTS-0C0A5C4500 (explanation)

> join the tenex-edge channel backlog

*Provider: claude | Session: 9a9fcbfd-06e5-404f-9dab-379cef2f5480 | Timestamp: 2026-06-30T19:36:37.504Z*

---

### LNK-CLIENTS-A42A8DED74 (explanation)

> list tenex-edge channels -- join the backlog one

*Provider: claude | Session: 984eda48-843c-4e9c-8f1e-8e2e03b16260 | Timestamp: 2026-06-30T14:58:54.243Z*

---

### LNK-CLIENTS-C27DD4A49A (requirement)

> my question is, how would you implement the app ../podcast if it were reimplemented in this framework launch an agent to evaluate that directory and another to evaluate /Users/pablofernandez/src/podcast-rmp (same question)

*Provider: codex | Session: 019e37a2-d4e8-7453-9542-b1f97f0a7173 | Timestamp: 2026-05-17T20:32:34.681Z*

---

### LNK-CLIENTS-C3BFD8A5F6 (explanation)

> tenex-edge channel --help

*Provider: codex | Session: 019ef8ee-cfce-75c1-9feb-d7cc5b869fea | Timestamp: 2026-06-24T09:24:48.580Z*

---

### LNK-CLIENTS-A13A3A47A7 (explanation)

> this works in Olas just fine ../Olas

*Provider: codex | Session: 019e3c2e-1d92-75c2-a9b9-d9781ffab391 | Timestamp: 2026-05-18T18:12:36.794Z*

---

### LNK-CLIENTS-C384780693 (explanation)

**Formulation 1 — 2026-06-30T15:02:32.848Z**

> using tenex-edge, join the backlog channel

*Provider: claude | Session: c128b504-b38a-4027-b505-74167b007921 | Timestamp: 2026-06-30T15:02:32.848Z*

**Formulation 2 — 2026-06-30T15:47:09.597Z**

> using tenex-edge, join the backlog channel

*Provider: claude | Session: c128b504-b38a-4027-b505-74167b007921 | Timestamp: 2026-06-30T15:47:09.597Z*

---

### LNK-CHIRP-C0C0158468 (rejection)

> 2309 won't land -- it shouldn't revert -- that was fucking wrong, any fixes on chirp code should happen in its own repo in ../chirp

*Provider: claude | Session: 47993c9a-0574-43f5-b298-7099185dbfdc | Timestamp: 2026-06-28T09:32:02.693Z*

---

### LNK-CHIRP-64B5236D02 (rejection)

**Formulation 1 — 2026-06-20T07:09:17.626Z**

> No orientations were specified in the io.f7z.chirp bundle.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 2 — 2026-06-20T07:09:17.626Z**

> A value for the Info.plist key 'CFBundleIconName' is missing in the bundle 'io.f7z.chirp'.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

---

### LNK-CLIENTS-A60528702A (correction)

> ah, so the old approach we have explicitly rejected?? no, fuck that, I don't want that shit anywhere -- if there's anything open still about that shit let's make sure it's properly cemtend so it doesn't resurface

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:35:44.135Z*

---

### LNK-CLIENTS-1A914F578B (rejection)

> 2. I think the internals might be too large, too complex and, for example, podcast-player is *barely* a nostr app, it barely uses nostr, yet it has to be tied to a nostr app framework -- that is just wrong -- an app shouldn't need to buy a whole way of archite

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T20:08:23.426Z*

---

### LNK-CLIENTS-786D6F2155 (rejection)

> And a second, smaller gap on top: no notion of "only keep feeds for rows actually on screen" (viewport-aware), so today it's all-or-nothing — fine for 6 groups, wasteful for 600.

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-09T13:50:25.722Z*

---

### LNK-CLIENTS-33A073B172 (rejection)

> don't add "superseding ADRs" to correct previous ADRs or any doc -- edit the doc and make it say the right thing -- persist this as a policy of the repo because this has happened many times and it makes no sense to carry forward incorrect data that is corrected in other docs.

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-19T22:28:32.152Z*

---

### LNK-CLIENTS-DD9A537466 (rejection)

> don't say "the checkout is dirty" -- review what's there, commit,finish, do whatever is needed -- you're the only one wroking on 29er!

*Provider: codex | Session: 019f1dd5-59c1-76a2-9d81-de6525c5be49 | Timestamp: 2026-07-01T21:06:54.592Z*

---

### LNK-CLIENTS-B758E58ADC (rejection)

> don't try to avoid breaking the build -- just delete whatever, and carry on -- if you try to fix the build while deleting you'll be wasting work (if that makes sense)

*Provider: codex | Session: 019f26fa-ad5a-7540-803f-773ab9ba27de | Timestamp: 2026-07-03T09:28:42.302Z*

---

### LNK-CLIENTS-4D587E179B (rejection)

> no, nip-89 is not used for this... look at how Olas does this in ../Olas

*Provider: codex | Session: 019e5e65-0b07-7cc3-9edc-d2dcd0bfc5c1 | Timestamp: 2026-05-25T09:16:47.900Z*

---

### LNK-CLIENTS-082E262FC0 (rejection)

> you keep running the same commands -- stop using claude -- instead use locally operated agents you control (using medium or high gpt 5.5 thinking models if you get to choose the model)

*Provider: codex | Session: 019ee178-1236-7913-a1e2-b49605f895a5 | Timestamp: 2026-06-20T22:20:01.050Z*

---
