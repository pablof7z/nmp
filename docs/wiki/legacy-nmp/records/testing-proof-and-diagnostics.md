---
title: "Testing, Proof, and Diagnostics"
source: "legacy-NMP forensic recovery"
record_count: 12
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Testing, Proof, and Diagnostics

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-PERF-D4CE30E784 (explanation)

> - Generated, vendored, lockfile, binary, and benchmark-output artifacts are exempt from the LOC ceiling.

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

---

### LNK-TESTING-10FFF8A54F (explanation)

> I think we can get rid of the fixture app (second app proof)

*Provider: claude | Session: 2cef5996-0123-4c16-ac16-1b318978ac9f | Timestamp: 2026-06-12T07:25:36.383Z*

---

### LNK-TESTING-2FF315EA43 (explanation)

> Additional ADRs should remain only if they own a live invariant that cannot be cleanly absorbed into that set or a durable doc. The burden of proof is on keeping an ADR, not deleting it.

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

---

### LNK-TESTING-91885F7562 (rationale)

> and do the initial deployment now so that I can test it

*Provider: codex | Session: 019e4bd9-8a71-7802-927b-f65b6f774b19 | Timestamp: 2026-05-21T21:24:45.233Z*

---

### LNK-TESTING-31C928C6E5 (explanation)

> - diagnostics as a permanent proof surface

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-TESTING-2D8316C26C (explanation)

> validate everything by reading code -- the docs might be lying

*Provider: codex | Session: 019e5543-1d4b-7591-9a21-76cbb8bfde3b | Timestamp: 2026-05-23T15:41:56.946Z*

---

### LNK-DIAGNOSTICS-20B06320B8 (rejection)

> is it normal having to publish so many crates independently? doesn't rust have something like a thing where you can enable different like "internal crates" or something like that? or would that be a bad design? I'm just surprised its such a struggle to publish these crates -- almost as if we're fighting what crates.io expects? (don't change anything, I'm just asking if this is the normal way of doing this or if we're doing something wrong)

*Provider: codex | Session: 019f24a6-20fe-7712-a709-7378284b85cd | Timestamp: 2026-07-03T17:39:18.272Z*

---

### LNK-DIAGNOSTICS-0F8B258416 (rejection)

> I'm not convinced -- if you would use (I'm not going to use it) a tool to debug the payloads, you might as well use a flatbuffer tool instead -- don't go there -- replace it with flatbuffers

*Provider: codex | Session: 019e6347-438f-7fa0-b12f-c21e4592f261 | Timestamp: 2026-05-26T08:22:09.286Z*

---

### LNK-PERF-8DE7A968E6 (rejection)

> no anything above a few seconds is not acceptable performance -- we need to investigate, profile, etc

*Provider: claude | Session: f308bb0b-7b74-4684-9a5b-1fce8ffcab35 | Timestamp: 2026-07-04T17:57:34.887Z*

---

### LNK-TESTING-E28E51D466 (rejection)

> Be concrete. Do not say "split by concern" — name the actual items, functions, structs, test groups that move together. Read the actual file contents before proposing.

*Provider: claude | Session: fe74166f-3d29-42a6-bac4-f962d4a2df0c | Timestamp: 2026-06-23T08:25:43.349Z*

---

### LNK-TESTING-BFB97FBA03 (rejection)

**Formulation 1 — 2026-07-01T11:09:35.633Z**

> I want to create a real e2e program that leverages this -- seed it with two pubkeys, make them follow a bunch of real pubkeys (source them from relay.primal.net using `nak`) -- then create a program that shows a feed of the pubkeys account1 uses and then add a way to switch to account2 -- the test would be that the feed would need to re-render with the feed of the people the active account follows -- all that without the program having to do anything special.

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T11:09:35.633Z*

**Formulation 2 — 2026-07-01T11:09:35.633Z**

> Make it in a way that I can test it -- don't commit this, this is just to validate it actually works in a real, simple, app, with real relay connectivity and whatnot

*Provider: codex | Session: 019f1c9f-048f-7613-bf1a-c63fb46bd780 | Timestamp: 2026-07-01T11:09:35.633Z*

---

### LNK-TESTING-E86FEC04EC (rejection)

> no "test helper" either -- stop adding hacks

*Provider: codex | Session: 019f17c6-74bc-74d1-8342-f5c4b7459b46 | Timestamp: 2026-06-30T09:28:35.646Z*

---
