---
title: "North Star and Scope"
source: "legacy-NMP forensic recovery"
record_count: 22
disposition: "historical-context"
authority: "direct-user-evidence"
---

# North Star and Scope

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-NORTH-9B8ACA5B95 (explanation)

**Formulation 1 — 2026-07-02T13:47:53.983Z**

> Protocol-specific docs
>   NIP-specific grammar, parser, artifact ownership, schema details.

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

**Formulation 2 — 2026-07-02T13:47:53.983Z**

> Specific cleanup guidance by cluster

*Provider: codex | Session: 019f2315-b653-73e0-8d01-b23ca5e053e3 | Timestamp: 2026-07-02T13:47:53.983Z*

---

### LNK-NORTH-72DA86C4DC (rationale)

> All the NMP primitives to make this work should be already available -- so if this is not trivial file as NMP bug because all this is supposed to already be easily possible.

*Provider: claude | Session: dcc80382-bcc0-45ea-8b9c-1a2fc741f872 | Timestamp: 2026-07-04T17:56:33.190Z*

---

### LNK-NORTH-150944994B (requirement)

> The Android app-loop FFI lane now runs on generated UniFFI bindings — AppHandle (lifecycle + dispatch_action_bytes(Vec<u8>)), UpdateSink callback interface, DispatchAck record — while FlatBuffers stays the byte payload (NMPD/NMPU, byte-for-byte) per ADR-0030. The 9 equivalent JNI app-loop entry points are deleted; KernelBridge.kt is a thin facade. D6 (errors-as-state, no throws), D8 (push-only), and quiescent teardown all hold. First real slice of the M14 epic.

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T19:34:23.749Z*

---

### LNK-NORTH-4E5C3CA4C5 (rationale)

> a different agent had a slightly different idea-- thoughts? Good instinct — an MVP is the right shape here precisely because the open questions are unknowns (does the hybrid work? is the Kotlin actually better?) rather than scope. You de-risk the decision cheaply instead of committing to 167 symbols on faith.

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T08:04:21.513Z*

---

### LNK-NORTH-57354F38EA (explanation)

> is the idea behind this thing to use nostr rust sdk or are we creating a new library? I'm not sure I follow whats the goal and architecture we have here

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T17:49:04.829Z*

---

### LNK-NORTH-8B7049CB9D (explanation)

> is the idea behind this thing to use nostr rust sdk or are we creating a new library? I'm not sure I follow whats the goal and architecture we have proposed here

*Provider: codex | Session: 019e370d-f271-7020-959c-0d584afa8a17 | Timestamp: 2026-05-17T17:49:11.514Z*

---

### LNK-NORTH-EF0CF8BFA0 (explanation)

> - Live query:
>     A Nostr Filter whose field values can be reactive Bindings. Delivered as native reactive primitives: Swift AsyncSequence / @Observable now, Kotlin Flow later.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-NORTH-10803DBE15 (explanation)

> Scope it on Android, exclude everything else

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T08:04:21.513Z*

---

### LNK-NORTH-0DD5B4FC2D (explanation)

> The UI framework owns rendering and observation scope.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-NORTH-2BF560D572 (explanation)

> Recommendation: the throwaway spike. The whole reason you're unsure is the hybrid + Kotlin-quality unknowns, and a 1–2 day spike converts those from speculation into measured numbers feeding an ADR — without breaking anything or committing the ecosystem to a migration mid-v1.

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T08:04:21.513Z*

---

### LNK-NORTH-1D263CB046 (explanation)

> - Codex design-first → no blocker
> - Sonnet implemented in an isolated worktree
> - Codex diff-review → caught 4 real BLOCKERs (teardown deadlock, lock-free UAF, debug_assert D6 violation, close()↔AutoCloseable collision) — all fixed with new concurrency tests
> - You decided the dispatch scope → kept JSON adapters as tracked residuals
> - All gates green: 38 crate tests, 98 doctrine, :app:assembleDebug builds, 28/28 CI checks pass

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T19:34:23.749Z*

---

### LNK-NORTH-06AF0004A6 (rationale)

> can we scope the work so that each gh issue can land on master asap so we don't drift off on an integration branch for a long time that will be really hard to merge? how much work is this whole epic? how much code can we expect to be able to remove (very rough estimate)

*Provider: codex | Session: 019f1cfb-f866-7b42-84f9-1c868c90cbb2 | Timestamp: 2026-07-01T20:23:16.242Z*

---

### LNK-NORTH-DEA4FD9D3B (explanation)

> there's a bunch of "Post-v1" issues in this "NMP v1 backlog"

*Provider: codex | Session: 019ecd38-851a-7233-8249-b8b46606db3d | Timestamp: 2026-06-16T07:10:34.149Z*

---

### LNK-NORTH-D127119EA6 (explanation)

> update any doc or plan that points to wasm being part of v1

*Provider: codex | Session: 019eb3a4-20f7-73a0-b092-dfb00836921b | Timestamp: 2026-06-11T08:50:22.062Z*

---

### LNK-NORTH-56CBD00507 (explanation)

> update the issue we're tracking, again, not with the solution, not even what codex suggested, but with the clearer scope of how big the architectural problem is

*Provider: claude | Session: 89c4af26-df83-4700-94cb-76cd12614c62 | Timestamp: 2026-06-28T10:40:04.621Z*

---

### LNK-SECURITY-2054B303E2 (explanation)

> auth token provided -- go ahead with setting up the github project now

*Provider: codex | Session: 019ecd38-851a-7233-8249-b8b46606db3d | Timestamp: 2026-06-16T06:35:31.923Z*

---

### LNK-NORTH-E37413CD22 (correction)

> This deliberately
> retired the old per-view bespoke doors (`open_author`, `open_thread`,
> `open_timeline`, etc.).

*Provider: codex | Session: 019f009b-7333-7800-b50f-643c41dd3c51 | Timestamp: 2026-06-25T21:08:52.943Z*

---

### LNK-NORTH-A3A0F65502 (rejection)

> Do not assume the current NMP architecture is correct. Do not assume NMP should be a standalone framework, a Dioxus integration, or part of another application runtime.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T19:53:53.382Z*

---

### LNK-NORTH-1E9D992EE6 (rejection)

> Do not assume the current NMP architecture is correct. Do not assume NMP should be a standalone framework, a Dioxus integration, or part of another application runtime.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T19:53:53.382Z*

---

### LNK-NORTH-5C591559D7 (rejection)

> Do not back away.

*Provider: codex | Session: 019f1cfb-f866-7b42-84f9-1c868c90cbb2 | Timestamp: 2026-07-01T20:04:51.268Z*

---

### LNK-NORTH-08ADE2B0BE (rejection)

> Recommend a preferred direction, but mention other viable options when the trade-offs are genuinely unresolved. Do not force false certainty.

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T19:53:53.382Z*

---

### LNK-NORTH-F2F2DE705C (rejection)

> ```text
> Trellis owns diff/scope/resource-plan logic.
> ReadSessionRegistry owns actual handles.
> No live handle stored in Trellis.
> No reconciliation from render/snapshot/projection closures.
> Parent scope close emits child-read closes.
> Open failure is reported bac

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-09T15:53:22.959Z*

---
