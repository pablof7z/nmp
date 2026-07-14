---
title: "Platform: iOS and SwiftUI"
source: "legacy-NMP forensic recovery"
record_count: 23
disposition: "legacy-evidence-archive"
authority: "legacy-extractor-assertion"
authorship: "consult-record-level-catalog"
---

# Platform: iOS and SwiftUI

Recovered historical context, rejected positions, superseded designs,
and mechanism rationale from the legacy NMP development record.
These are preserved for chronology, not as current requirements.

## Records

### LNK-IOS-BC2A0DC7BE (explanation)

**Formulation 1 — 2026-06-30T11:32:48.805Z**

> - #2531 — nmp-content display-separation; strips author_display_name/author_picture_url from non-Profile projections + .fbs (coordinated wire break, SCHEMA_VERSION bumped, all Rust/web/Swift/Kotlin consumers regenerated).

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

**Formulation 2 — 2026-06-30T11:32:48.805Z**

> ProfileProjection preserved.

*Provider: codex | Session: 019f174d-3648-7021-a77d-615bdcb61071 | Timestamp: 2026-06-30T11:32:48.805Z*

---

### LNK-IOS-5BEE7F767E (explanation)

> 5. #2224: unify identicon algorithm across iOS/Android
>      Clear target: one 5x5 symmetric algorithm. Slightly more
>      surface area, but well specified.

*Provider: claude | Session: f8a28ec2-1caf-4b06-a687-07dcb466ca6d | Timestamp: 2026-06-28T06:01:49.030Z*

---

### LNK-IOS-D9220CE799 (explanation)

> Add public Swift API:
>      - Diagnostics.swift value types
>      - DiagnosticsQuery.swift
>      - NMPDiagnostics: AsyncSequence
>      - NMPEngine.observeDiagnostics() throws -> NMPDiagnostics
>   6.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-IOS-FEA97461EA (explanation)

**Formulation 1 — 2026-07-11T07:33:08.090Z**

> - Live query:
>     A Nostr Filter whose field values can be reactive Bindings. Delivered as native reactive primitives: Swift AsyncSequence / @Observable now, Kotlin Flow later.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

**Formulation 2 — 2026-07-11T07:33:08.090Z**

> Regenerate Swift bindings / xcframework if the FFI surface changed.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

**Formulation 3 — 2026-07-11T07:33:08.090Z**

> Add public Swift API:
>      - Diagnostics.swift value types
>      - DiagnosticsQuery.swift
>      - NMPDiagnostics: AsyncSequence
>      - NMPEngine.observeDiagnostics() throws -> NMPDiagnostics
>   6.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

**Formulation 4 — 2026-07-11T07:33:08.090Z**

> Add Swift package tests that observeDiagnostics yields a snapshot.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-IOS-623BC83BCD (explanation)

**Formulation 1 — 2026-06-20T07:09:17.626Z**

> For more information see http://help.apple.com/xcode/mac/current/#/dev10510b1f7.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 2 — 2026-06-20T07:09:17.626Z**

> For more information see http://help.apple.com/xcode/mac/current/#/dev10510b1f7.";
> }’.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

---

### LNK-IOS-D6AFD436F7 (requirement)

**Formulation 1 — 2026-06-20T07:09:17.626Z**

> For more information see http://help.apple.com/xcode/mac/current/#/dev10510b1f7.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 2 — 2026-06-20T07:09:17.626Z**

> For more information see http://help.apple.com/xcode/mac/current/#/dev10510b1f7.";
> }’.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 3 — 2026-06-20T07:09:17.626Z**

> -- send a sonnet agent to fix in this master branch and commit its work directly

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

---

### LNK-IOS-50FE6404D3 (explanation)

> WHY ON FUCKING EARTH ARE YOU STILL DEPENDING ON ME TO DEBUG YOUR UTTERLY BROKEN SHIT?! I TOLD YOU TO FUCKING SEED THE FUCKING APP WITH MY LIST OF FUCKKING SHIT AND YOU ITERATE ON A FUCKING SIMULATOR!!!!!

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-10T14:03:21.286Z*

---

### LNK-IOS-2D7E222E16 (explanation)

> Current milestone truth from /Users/pablofernandez/Work/nmp:
>   - M0 founding gate passed.
>   - M1 grammar engine proved.
>   - M2 compiler/router/coalescing proved.
>   - M3 store + transport + write outbox + negentropy proved.
>   - M4 Swift SDK boundary proved.
>   - M5 iOS falsifier app is next.
>   - M6 Android/Kotlin is not started.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-IOS-E275238222 (explanation)

**Formulation 1 — 2026-07-11T07:33:08.090Z**

> Current milestone truth from /Users/pablofernandez/Work/nmp:
>   - M0 founding gate passed.
>   - M1 grammar engine proved.
>   - M2 compiler/router/coalescing proved.
>   - M3 store + transport + write outbox + negentropy proved.
>   - M4 Swift SDK boundary proved.
>   - M5 iOS falsifier app is next.
>   - M6 Android/Kotlin is not started.

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

**Formulation 2 — 2026-07-11T07:33:08.090Z**

> Important current M5 context:
>   The M5 falsifier app is the thesis gate. It must be a small idiomatic SwiftUI app, not derived from old NMP apps, using NMP as a normal library. The human judgment is: does this feel
>   like a native library, or a framework in disguise?

*Provider: codex | Session: 019f5007-c99a-7890-8cb3-dbafe817a501 | Timestamp: 2026-07-11T07:33:08.090Z*

---

### LNK-IOS-A8F974203E (rationale)

> None of the current NMP apps are useful to drive because they are NMP apps and that invalidates the exercise; they shouldn’t need to be NMP apps, just like they aren’t TanSatck Query apps or whatever is the equivalent for iOS/android/desktop. Right?

*Provider: claude | Session: fc712473-8d8b-4d5f-86cd-61b78dbc8b41 | Timestamp: 2026-07-10T20:40:52.132Z*

---

### LNK-IOS-23C16201B6 (requirement)

> Once you’re done with everything and verified things cut a new version of NMP and update all the NMP  apps to use it and push all the iOS apps to my iPhone (chirp, highlighter, tenex-off, podcast-player) - do this via background agents (my iPhone will be locked, just install)

*Provider: claude | Session: ab8061fc-b277-4ba4-bf55-1532bcb1aa90 | Timestamp: 2026-06-14T22:00:51.369Z*

---

### LNK-IOS-65FA3ED1AF (explanation)

**Formulation 1 — 2026-06-20T07:09:17.626Z**

> The bundle does not contain an app icon for iPad of exactly '152x152' pixels, in .png format for iOS versions >= 10.0.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 2 — 2026-06-20T07:09:17.626Z**

> The bundle does not contain an app icon for iPhone / iPod Touch of exactly '120x120' pixels, in .png format for iOS versions >= 10.0.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

---

### LNK-IOS-27B39728E6 (explanation)

**Formulation 1 — 2026-06-20T07:09:17.626Z**

> The bundle does not contain an app icon for iPad of exactly '152x152' pixels, in .png format for iOS versions >= 10.0.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

**Formulation 2 — 2026-06-20T07:09:17.626Z**

> The bundle does not contain an app icon for iPhone / iPod Touch of exactly '120x120' pixels, in .png format for iOS versions >= 10.0.

*Provider: claude | Session: a928d155-db8c-4909-a420-4db220ff697a | Timestamp: 2026-06-20T07:09:17.626Z*

---

### LNK-IOS-655DFABAD5 (explanation)

> - Wired the UniFFI drift script into CI (codegen-drift.yml) so bindings can't silently desync — closing a gap the original work left open.
> - Filed #2145 (M14-1) for the follow-up: migrate Android write verbs to GeneratedActionBuilders bytes-only.
> - Updated parent #2125 with the outcome and the recommended next lanes (M14-1, then iOS app-loop, then capabilities).

*Provider: codex | Session: 019f02e0-e7fe-7b52-a283-384b224b5260 | Timestamp: 2026-06-26T19:34:23.749Z*

---

### LNK-IOS-38CDD44023 (decision)

> delegate to fable to decide what to do, how to do it, whether the design is ther right one and how to approach things

*Provider: claude | Session: f308bb0b-7b74-4684-9a5b-1fce8ffcab35 | Timestamp: 2026-07-04T10:42:09.204Z*

---

### LNK-IOS-5E70F5457C (explanation)

> events show up very slow coming in on the ios app

*Provider: codex | Session: 019e37dc-31b6-7da0-a70a-82940004dc32 | Timestamp: 2026-05-17T21:34:13.972Z*

---

### LNK-IOS-E00B9BA61F (requirement)

> ignore the web part for hl -- just focus on the ios app -- and whatever is dirty, I have no idea what that work is -- review commit/discard -- do whatever

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-09T20:11:59.512Z*

---

### LNK-IOS-73F8059E07 (requirement)

> lets write a specific milestoned plan for this support -- I want to start shipping reusable components that apps can install/update later, starting with content rendering since its the most complex stuff and most needed throughout apps -- write and commit a plan to implement this, starting with content rendering support for ios and android -- ideally we can have different content rendering widgets, for example a minimalistic mention that is used when "hey nostr:npub1..." and other variation that is richer with press-and-hold to view a preview of the user for example...

*Provider: codex | Session: 019e5e57-b1a4-7963-8823-b58167d49ff7 | Timestamp: 2026-05-25T09:15:37.030Z*

---

### LNK-IOS-644301CF57 (explanation)

> skip the web part, we can focus on the ios and android for now

*Provider: codex | Session: 019e5543-1d4b-7591-9a21-76cbb8bfde3b | Timestamp: 2026-05-25T08:04:26.349Z*

---

### LNK-IOS-82983E7F25 (explanation)

> the app is on the ios sim and completely frozen

*Provider: claude | Session: b27e1870-72fa-4315-b66c-dd5a2e61a6fe | Timestamp: 2026-07-10T18:10:04.448Z*

---

### LNK-IOS-A376C6E15F (rationale)

> what does "This exists because those namespaces don't have ACTION_BUILDERS entries yet and therefore have no generated Swift builders" mean? what's missing?

*Provider: claude | Session: 0e697f02-d08a-420c-9dbf-77e7bf28276a | Timestamp: 2026-06-28T08:10:49.674Z*

---

### LNK-IOS-6F6754D1E2 (rationale)

> what does chirp ios use to display the relays we are connecte to? because I see logs that its connecting to other relays but I only see two relays connected on the diagnostics tab

*Provider: codex | Session: 019e5e80-6392-7a03-8500-fce99951e5f2 | Timestamp: 2026-05-25T09:39:48.845Z*

---

### LNK-IOS-C6685FDBD2 (rejection)

> 1. people I'm following - I go to their profile, the button shows "Follow"
> 2. when I hit the follow button on someone's profile that I don't follow I get the haptic feedback but nothing happens (the follow button continues to say "follow" -- I don't know if it actually added the user to my follows list)

*Provider: claude | Session: e6b44a84-8cfc-48b2-863a-58382398b5df | Timestamp: 2026-06-19T12:17:31.319Z*

---
