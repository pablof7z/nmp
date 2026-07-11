# NMP Builder Manual

NMP is an **embeddable Nostr sync-and-routing engine** — a library your app talks to, not a framework your app lives inside. Its entire app-facing surface is **two nouns**: a **live query** you observe (a Nostr `Filter` whose field values are `Binding`s) and a **write intent** you publish (a template plus a durability and routing class). Everything hard about Nostr — outbox routing, coverage, dedup, replaceable supersession, offline sync — lives inside the engine; everything downstream of delivery is your code. The governing rule is **values in, code after**.

## Start here

- **[Build a working timeline in 10 minutes](04-ten-minute-timeline.md)** — a running iOS app against real relays, proven by reading the diagnostics screen. If you want to *build* something right now, start here.
- **[The mental model in one diagram](02-mental-model.md)** — values in → engine → rows + coverage out → your code after. If you want the conceptual spine first, start here.

New to the terms? [What works today vs. what's coming](03-status-map.md) carries the full BUILT/PLANNED map plus a glossary of every Nostr and NMP term the manual uses.

## Status labels

**BUILT** — running and independently verified; examples are real and runnable (Swift SDK or Rust `Handle`/`nmp-demo` today). **PARTIAL** — some of it works; the chapter says which. **PLANNED** — design preview of the intended shape, not yet shipped. Only the **Swift and Rust** SDKs are BUILT today; Kotlin/TS/TUI examples are intended-shape.

## Contents

### Part I — Orient
- **[01 · Why NMP exists](01-why-nmp.md)** — the problem, library-not-framework, why-not-NDK/Applesauce/roll-your-own · *BUILT*
- **[02 · The mental model in one diagram](02-mental-model.md)** — values in → engine → rows + coverage out → your code after · *BUILT*
- **[03 · What works today vs. what's coming](03-status-map.md)** — the BUILT/PLANNED map + glossary of Nostr & NMP terms · *BUILT*

### Part II — Get running
- **[04 · Build a working timeline in 10 minutes](04-ten-minute-timeline.md)** — keystroke tutorial, iOS · *BUILT* ★
- **[05 · The two nouns and the ownership table](05-two-nouns.md)** — engine / app / UI framework, who owns what · *BUILT*
- **[06 · Your first app in 20 lines](06-first-app.md)** — per-platform, the shape · *BUILT* ★
- **[07 · Adding NMP to an app you already own](07-brownfield.md)** — brownfield coexistence · *BUILT* ★
- **[08 · Packaging, build & distribution](08-packaging.md)** — xcframework / cargo-ndk / wasm / SwiftPM · *BUILT*

### Part III — Reading (queries & results)
- **[09 · Live queries & the binding grammar](09-binding-grammar.md)** — `$myFollows`, groups-I'm-in, follows−mutes worked · *BUILT* ★
- **[10 · Consuming results](10-consuming-results.md)** — rows, snapshots, and presentation ownership · *BUILT* ★
- **[11 · Coverage: empty vs unknown](11-coverage.md)** — the trust chapter · *BUILT* ★
- **[12 · Feeds & the Collection observation mode](12-collection-mode.md)** — ordering, windows, pagination · *PLANNED*
- **[13 · Delivery-side transforms](13-delivery-transforms.md)** — WoT filtering & custom sort · *PLANNED*

### Part IV — Writing
- **[14 · Writing: intents, receipts, and the durability guarantee lattice](14-writing.md)** · *BUILT* ★
- **[15 · Editing replaceable state safely](15-editing-replaceable.md)** — the wiped-follow-list trap · *PARTIAL*

### Part V — The hard concerns
- **[16 · Identity & multi-account](16-identity.md)** — re-root in one line · *BUILT* ★
- **[17 · Relays: outbox, indexers, and roles](17-relays.md)** — you never pick relays · *BUILT*
- **[18 · "Where did my query go?"](18-tracing-demand.md)** — tracing demand through the compiler · *BUILT* ★
- **[19 · Offline & sync](19-offline-sync.md)** — negentropy, coverage watermarks, the limits of replay · *BUILT*
- **[20 · Capabilities](20-capabilities.md)** — signer, AUTH policy, encrypt/decrypt · *PARTIAL*
- **[21 · Provenance](21-provenance.md)** — and why private events can't be republished · *BUILT*

### Part VI — Operate
- **[22 · Diagnostics & debugging](22-diagnostics.md)** — "why is my feed empty?" · *BUILT* ★
- **[23 · Threading, the main-thread contract & app lifecycle](23-threading-lifecycle.md)** · *BUILT*
- **[24 · Cost & performance](24-performance.md)** — the pay-as-you-go mental model · *BUILT*
- **[25 · Testing an app that embeds NMP](25-testing.md)** · *BUILT*
- **[26 · Troubleshooting & FAQ](26-troubleshooting.md)** — each answer read off the diagnostics · *BUILT*

### Part VII — Reference & judgment
- **[27 · The batteries: recipes catalog + choosing](27-recipes-and-choosing.md)** — recipe vs compose vs own · *PARTIAL*
- **[28 · Patterns & anti-patterns](28-patterns.md)** — the bug-class ledger as DX · *BUILT*
- **[29 · What NMP does NOT do (and why)](29-not-do.md)** · *BUILT*
- **[30 · Platform SDK guides](30-platform-guides.md)** — iOS, Android, Rust, TS, TUI · *BUILT* ★
- **[31 · Example gallery](31-gallery.md)** — + graduating from the falsifier app · *BUILT*
- **[32 · Extending NMP](32-extending.md)** — protocol modules & recipes under the five tests · *PLANNED*
- **[33 · Versioning & stability](33-versioning.md)** — what "provisional-until-v2" means for you · *BUILT*

---

<sub>Front matter for authors: [design guidelines & the boundary/modularity principles](000-design-guidelines-and-toc.md) · [writing brief & canonical TOC](001-writing-brief.md). Everything here is provisional until v2.0; where a chapter and [`../../README.md`](../../README.md) disagree, the repo README's live status table wins.</sub>
