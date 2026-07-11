# What works today vs. what's coming

**Status: BUILT** (this chapter is the manual's own truth anchor; where it and [`README.md`](../../README.md) disagree, the README's live status table wins.)

After this chapter you'll know exactly which parts of NMP are running today and which are design-preview, so you never build on something that isn't there — and you'll have a glossary that grounds every Nostr and NMP term the rest of the manual uses.

NMP is a **day-0 greenfield rebuild**. Everything is provisional until a v2.0 ships (not before Aug 2026); nothing is self-compatibility-binding before then. This manual inherits the project's truth-anchor discipline: every chapter states whether its subject is BUILT, PARTIAL, or PLANNED, and never claims a mechanism holds before running code proves it.

## What the labels mean

- **BUILT** — running and independently verified. Examples in the chapter are real and runnable (against the Swift SDK or the Rust `Handle`/`nmp-demo` CLI today).
- **PARTIAL** — some of it works; the chapter says exactly which part, and marks the rest.
- **PLANNED** — design preview. The chapter shows the *intended* shape, clearly labeled "not yet shipped." You can design against it, but you can't run it yet.
- **PLANNED-shape** — the concept is BUILT on one platform but the *idiom* shown for another (Kotlin `Flow`, TS async iterator, TUI) isn't built for that platform yet. Only **Swift and Rust** SDKs are BUILT today.

## Milestone state (the ground truth)

From the README's status table — the single source of truth for build state:

| Milestone | State |
|---|---|
| M0 — Founding gate (grammar + two-noun surface) | **PASSED** (amendments applied) |
| M1 — Grammar engine spike | **PROVED** (independently verified) |
| M2 — Compiler/router + coalescing | **PROVED** (independently verified) |
| M3 — Store + transport + write outbox | **PROVED** (independently verified) |
| M4 — Swift SDK boundary | **PROVED** (live-proven through Swift) |
| M5 — iOS falsifier app | **built & running** on the simulator (live feed from 2 indexers); the thesis-gate *judgment* — "library or framework?" — is the owner's pending call |
| M6 — Android (Kotlin/Flow) | not started |

Concretely, this means: the reactive binding grammar is proven general at two depths; routing and widen-only coalescing are proven; the store persists and stays authoritative offline (cold-start offline reads return `CompleteUpTo`; never-synced returns `Unknown`); durable writes get per-relay acks; and a Swift `AsyncSequence` SDK reads like a native library over a live relay. A Rust demo CLI (`nmp-demo`) runs the whole engine against the *live* Nostr network — self-bootstrapping outbox from two indexer relays, thousands of real notes, no fixtures.

The permanent per-relay/per-kind **diagnostic surface** the MVP requires has landed (engine + FFI, live-proven; the Falsifier's diagnostics screen ships today — see *[Diagnostics & debugging](22-diagnostics.md)*). The **falsifier app** is built and runs on the simulator with a live feed from two indexer relays; what's still open on M5 is the *thesis-gate judgment* — whether a normal iOS developer experiences NMP as a library, not a framework (the owner's call on the running app). **Not yet built:** **Android** (M6). **Disproved so far:** nothing.

## Chapter status map

Every chapter's subject, with its status. Use this as the index of what you can rely on today.

### Part I — Orient
| Ch | Subject | Status |
|---|---|---|
| 01 | Why NMP exists | **BUILT** (conceptual) |
| 02 | The mental model | **BUILT** (conceptual) |
| 03 | This status map + glossary | **BUILT** |

### Part II — Get running
| Ch | Subject | Status |
|---|---|---|
| 04 | Build a working timeline in 10 minutes (iOS) | **BUILT** ★ |
| 05 | The two nouns & the ownership table | **BUILT** |
| 06 | Your first app in 20 lines (per-platform shape) | **BUILT** (Swift + Rust; Kotlin/TS PLANNED-shape) ★ |
| 07 | Adding NMP to an app you already own | **BUILT** ★ |
| 08 | Packaging, build & distribution | **BUILT** (Swift/Rust; wasm PLANNED) |

### Part III — Reading (queries & results)
| Ch | Subject | Status |
|---|---|---|
| 09 | Live queries & the binding grammar | **BUILT** ★ |
| 10 | Consuming results: rows, snapshots, presentation ownership | **BUILT** ★ |
| 11 | Coverage: empty vs unknown | **BUILT** ★ |
| 12 | Feeds & the Collection observation mode | **PLANNED** (Tier-A pending; design-preview) |
| 13 | Delivery-side transforms (WoT, custom sort) | **PLANNED** |

### Part IV — Writing
| Ch | Subject | Status |
|---|---|---|
| 14 | Writing: intents, receipts, durability lattice | **BUILT** (pre-signed publish across FFI PARTIAL) ★ |
| 15 | Editing replaceable state safely | **PARTIAL** (safe pattern documentable now; structural fix PLANNED) |

### Part V — The hard concerns
| Ch | Subject | Status |
|---|---|---|
| 16 | Identity & multi-account | **BUILT** ★ |
| 17 | Relays: outbox, indexers, roles | **BUILT** (live-proven) |
| 18 | "Where did my query go?" — tracing demand | **BUILT** ★ |
| 19 | Offline & sync: negentropy, watermarks | **BUILT** |
| 20 | Capabilities: signer, AUTH, encrypt/decrypt | **PARTIAL** (local nsec signer BUILT; NIP-46, NIP-42 AUTH, decrypt path PLANNED) |
| 21 | Provenance; why private events can't be republished | **BUILT** |

### Part VI — Operate
| Ch | Subject | Status |
|---|---|---|
| 22 | Diagnostics & debugging | **BUILT** (engine + FFI diagnostic surface landed and live-proven; the Falsifier diagnostics screen ships today) ★ |
| 23 | Threading, main-thread contract, lifecycle | **BUILT** |
| 24 | Cost & performance: pay-as-you-go | **BUILT** |
| 25 | Testing an app that embeds NMP | **BUILT** |
| 26 | Troubleshooting & FAQ | **BUILT** |

### Part VII — Reference & judgment
| Ch | Subject | Status |
|---|---|---|
| 27 | The batteries: recipes catalog + choosing | **PARTIAL** (recipe *shape* settled; per-NIP module mechanism PLANNED) |
| 28 | Patterns & anti-patterns (the bug-class ledger as DX) | **BUILT** |
| 29 | What NMP does NOT do (and why) | **BUILT** |
| 30 | Platform SDK guides (iOS, Android, Rust, TS, TUI) | **BUILT** for iOS + Rust/TUI; Android/TS PLANNED ★ |
| 31 | Example gallery + graduating from the falsifier | **BUILT** (falsifier itself M5) |
| 32 | Extending NMP: protocol modules & recipes | **PLANNED** |
| 33 | Versioning & stability | **BUILT** |

One honest note the table above encodes: **Chapter 27 (recipes)** and the **modularity mechanism** (per-NIP modules / feature flags) are settled in *principle* but the packaging mechanism isn't finalized — the shape is right, the switch isn't wired. (Chapter 22's diagnostic surface, by contrast, is fully BUILT: engine, FFI, and the shipping Falsifier screen.)

## Glossary — Nostr terms

Terms from the protocol itself. If you're migrating from NDK or Applesauce these are familiar; if you're new to Nostr, this is your grounding.

- **Event** — the one data structure in Nostr. A signed JSON object with a `kind`, `content`, `tags`, `created_at`, an author `pubkey`, and an `id` (hash of the rest). Everything — a note, a profile, a follow list, a reaction — is an event distinguished by its **kind**.
- **kind** — an integer naming what an event *is*. kind:1 = a text note; kind:0 = a profile (metadata); kind:3 = a follow (contact) list; kind:7 = a reaction; kind:10002 = a relay list. Ranges matter: `0`, `3`, `10000`–`19999` are replaceable; `30000`–`39999` are *addressable* (replaceable, keyed also by a `d`-tag).
- **NIP** — *Nostr Implementation Possibility*, a numbered spec defining a convention: NIP-01 (the core protocol), NIP-10 (reply threading via `e`/`p` tag markers), NIP-51 (lists), NIP-65 (the outbox model / relay lists), NIP-29 (relay-based groups), NIP-42 (relay AUTH), NIP-46 (remote signing), NIP-50 (search), NIP-77 (negentropy sync). A **protocol fact** the manual encodes; a NIP module is how non-core NIP support is added (see *modularity*).
- **relay** — a WebSocket server that stores and serves events. There is no central one; clients talk to many. You send a `REQ` (subscribe with a filter) and get matching events plus stored history.
- **filter** — the query object you send in a `REQ`: `{ kinds, authors, "#e"/"#p"/... tags, since, until, limit, search? }`. A relay returns events matching it. In NMP, a filter's field *values* can be `Binding`s (see *live query*).
- **outbox (model)** — NIP-65's routing rule: each user publishes a relay list (kind:10002) declaring their **write relays**; to read someone's events you go to *their* write relays, not a shared pool. Correct fan-out is the outbox model applied with a covering set and a cap.
- **replaceable** — an event kind where a newer event *supersedes* older ones (by `created_at`, lexicographically-smallest-`id` tiebreak) rather than adding to them. Profiles, follow lists, relay lists, and all addressable kinds are replaceable. Storing stale copies is a classic bug.
- **negentropy** — a set-reconciliation protocol (NIP-77) that lets a client and relay efficiently compute *which events each is missing* without re-sending everything. NMP syncs negentropy-first against relays whose NIP-77 support it has probed.
- **gift-wrap** — the NIP-59 envelope (kind:1059) that wraps an encrypted event inside another encrypted, ephemerally-signed event, hiding metadata (sender, kind, timing) — the basis for private DMs and private lists. Decryption needs the key, which is why NMP has an engine-internal decrypt capability.
- **npub / nsec / hex** — encodings of keys. A **pubkey** is 32 bytes; `npub1…` is its bech32 display form, and 64-char **hex** is its raw form. `nsec1…` is a *secret* key. The engine works in hex and emits hex; turning it into an `npub` or a display name is presentation — your job.

## Glossary — NMP terms

Terms this engine introduces. These are the working vocabulary of the whole manual.

- **live query** (the read noun) — a Nostr `Filter` whose field values are `Binding`s, handed to `observe`. A plain, hashable, serializable *value*. The engine keeps it correct as its inputs change.
- **write intent** (the write noun) — a durable, acknowledged write: an unsigned template + a **durability class** + a **routing class**. Returns a streaming receipt. Never a fire-and-forget publish.
- **Binding** — what a filter field value can be: `Literal(set) | Reactive(ActivePubkey) | Derived(inner: Filter, project: Selector) | SetOp(Union|Intersect|Diff, [Binding])`. The grammar that makes a query *reactive on the demand side*.
- **Selector** — the closed vocabulary for projecting a `Derived` binding's inner rows into an outer field: `Authors | Ids | Tag(char) | AddressCoord`. **Closed and introspectable — never an app closure.** Extend-don't-escape: a need outside it extends the vocabulary (a design event), never admits code.
- **Reactive(ActivePubkey)** — "whoever the active signer is, right now." The single place account context lives; identity switching re-roots every binding that mentions it.
- **coverage** — the trust state delivered alongside rows: `Unknown | CompleteUpTo(watermark)`. `Unknown` = "can't yet know" (show a spinner); `CompleteUpTo` = "provably complete to this point" (an empty result here is *authoritative*). The difference between "empty" and "unknown," kept as distinct types.
- **watermark** — the proof carried by `CompleteUpTo`: the marker showing a (filter, relay) window has been synced completely. A cache miss is authoritative only when a watermark proves it.
- **capability** — a pluggable engine-side ability you configure but the engine *invokes*: the **signer**, the **encrypt/decrypt** key-holder, the **AUTH policy** (a declarative value, not a callback). You choose which one exists; the engine chooses when it runs.
- **recipe** — a blessed convenience: a pure, *printable* function returning a `Filter`/`WriteIntent`/observation descriptor built entirely from public values, encoding a **protocol fact** (kinds/tags/NIP shape), that a second unrelated app would write identically. `.profile(pubkey)`, `.reactions(to:)`, `.textNote(content:)`. Never the *only* door to what it wraps; always deletable. Ships in a NIP module, not core.
- **module** — an opt-in unit of non-core protocol meaning (a per-NIP crate / feature flag / registerable unit). You link only the modules you enable; a minimal app carries zero reaction code. The *modularity principle* — pay only for the NIPs you use.
- **lane** — a typed reason a relay is in a route: NIP-65, hint, provenance, or user-configured. The compiler routes over lane-typed facts; you never pass a `relays:` list (there is no such parameter).
- **diagnostic surface** — the read-only projection of engine state: per relay and per kind, the exact filters sent, events received, and coverage proven. The acceptance test made visible; how you debug NMP (by *reading*, not printf).
- **Collection observation mode** — an opt-in *mode* of the live query (not a third noun) adding engine-maintained ordering, a bounded window, and `loadMore` pagination over the same demand node. PLANNED. `OrderKey`/`RowKey` are closed vocabularies, never app comparators.
- **re-root** — what account-switching does: replace the identity root of the binding graph, tearing the old account's demand down (reverse-of-open, exactly-once) *before* activating the new — so no stale subscription survives to leak into.
- **demand** — the resolved set of "what to actually subscribe to," computed by the engine from your live queries. You declare intent; the engine owns demand. Refcounted: identical descriptors share one graph node; the last observer dropping withdraws demand (debounced).

## What to read next

You now have the map and the vocabulary. If you want to *build* something immediately, jump to *[Build a working timeline in 10 minutes](04-ten-minute-timeline.md)*. If you want the conceptual spine first, read *[The two nouns & the ownership table](05-two-nouns.md)* and then *[Live queries & the binding grammar](09-binding-grammar.md)* — the crown-jewel chapter everything else orbits.

---

<!-- nav-footer -->
<sub>← [The mental model](02-mental-model.md) · [Index](README.md) · [Timeline in 10 minutes](04-ten-minute-timeline.md) →</sub>
