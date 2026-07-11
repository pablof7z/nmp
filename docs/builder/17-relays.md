# Relays: compiled routes and typed protocol context

**Status: CURRENT + TARGET.** The coverage router, lane vocabulary, and
self-bootstrapping author outbox are built. Module-owned contextual publication
authority, including the governed NIP-29 composition path, remains target work.

After this chapter you will understand why generic observe/publish calls accept
no app-expanded relay list, how typed facts contribute routes, and why a
protocol host relay is not a generic override.

## Generic calls have no `relays:` parameter

`observe(_:)` takes demand and generic `publish(_:)` takes an intent; neither
takes an app-computed relay array. This is ledger #3: routing is compiler output
from typed facts, not a caller bypass.

One current operator input is a typed set of **indexer relays** at construction:

```swift
let nmp = try NMPEngine(config: .init(
    storePath: cachePath,
    indexerRelays: ["wss://relay.damus.io", "wss://purplepag.es"]
))
```

These are discovery policy, not "the relays content comes from." The engine
uses them to learn author outboxes. Their configured count is operator policy,
not a proof that every requested author can be covered.

A typed protocol operation may also contribute a relay fact when the protocol
defines that authority. For example, publishing through a NIP-29 group may add
`HostRelay(group, relay)` alongside the required `h` tag. That contribution is
validated, inspectable, and visible in diagnostics. It does not create a raw
route-list parameter or give NIP-29 ownership of the foreign event kind.

## Lanes: the closed vocabulary of "why this relay"

Every relay the engine considers carries a **lane** — a typed reason it is a candidate. The lane set is a closed enum (`nmp_router::Lane`); you extend it by a design event, never by passing a free-form string:

| Lane | What it means |
|---|---|
| `Nip65Write` | The author's kind:10002 WRITE relay — the outbox default |
| `Hint` | A relay hint from a tag / `nevent` / `nprofile` |
| `Provenance` | Where we have previously seen this author's events |
| `UserConfigured` | Operator policy — role-tagged config, *not* a route override |
| `IndexerDiscovery` | The operator indexer set — discovery kinds ONLY, never a content fallback |
| `GroupHost` | A NIP-29 group host relay for a non-author (pinned) atom |
| `DmInbox` | A kind:10050 DM inbox (pinned) |

Two things to notice. First, `UserConfigured` and protocol context are typed
facts, not route callbacks. Second, `IndexerDiscovery` is fenced: it is eligible
only for discovery-kind atoms and is never a generic content fallback. If an
author's outbox is unknown, that author demand remains explicitly uncovered
until discovery supplies a valid candidate.

## Roles are additive

The single most important thing to internalize: **a relay's roles stack.** A relay is not "an indexer" *or* "a write relay." The same URL can be both, and when it is, it does both jobs.

Discovery kinds are `{0, 3}` plus the whole NIP-01 replaceable range
`10000..=19999` (`DiscoveryKinds::default`). An atom whose kinds are all
discovery kinds *may* use `IndexerDiscovery`. An arbitrary non-discovery kind,
such as `9999`, may not. Eligibilities are computed independently and unioned:

```rust
// route::build_candidates, paraphrased:
let mut list = dir.write_relays(author);      // this author's kind:10002 writes
list.extend(dir.extra_relays(author));        // hints / provenance / config
if is_discovery {                             // AND, only for discovery atoms:
    list.extend(indexers.map(|u| LanedRelay::new(u, Lane::IndexerDiscovery)));
}
```

So if `wss://purplepag.es` is one of your configured indexers **and** it also appears in author A's kind:10002 write list, then:

- A's kind:3 / kind:0 discovery atom routes there via the `IndexerDiscovery` lane, *and*
- A's kind:9999 atom *also* routes there — via `Nip65Write`, because it is
  genuinely A's outbox.

The relay carries `0/3/1xxxx` because it is an indexer, and it *additionally* carries A's content because it is in A's mailbox. Nothing about being an indexer excludes it from also being a content relay. The router's own `additive_relay_role_*` tests pin this exact property.

## Self-bootstrapping: two indexers become the whole outbox graph

Here is the mechanism that lets you configure two relays and never think about relays again. At startup the engine's relay directory (`LiveDirectory`) knows *only* your indexers. Every author's write relays start empty. The engine fills them in over time, entirely on its own.

The internal loop (`EngineCore::sync_discovery`, run on every recompile):

1. Look at current demand. For every author referenced whose write relays are still unknown, the engine opens an **internal kind:10002 discovery subscription** covering exactly those authors.
2. That discovery atom (`kinds:[10002], authors:{unknown}`) is just another entry in the demand set — it is a discovery kind, so the *existing* router eligibility routes it to your indexers. No special path.
3. When an author's kind:10002 arrives, the engine re-reads the store's current winner for it and feeds the write relays into the directory (`ingest_write_relays`).
4. The very next recompile routes that author's *content* atoms to their now-known write relays — never to the indexer.

The headline falsifier test walks this exactly. Configure a single indexer, follow author A, then:

- **Before A's kind:10002 arrives:** the engine opens its internal discovery
  demand and A's arbitrary non-discovery atom is routed nowhere.
- **After A's kind:10002 lands** (declaring write relay R): that atom routes to
  R and still not to the indexer merely because it is an indexer.

That is the author-outbox path: the app declared a selection; the engine
discovered where A writes and compiled the route.

The discovery subscription is deliberately **widen-only**: once an author is in it, they stay in it even after resolving, and it only tears down when *no* author needs discovering. This is not sloppiness — reopening a narrower subscription on every author-resolves-one-at-a-time event caused a real triangular-number over-fetch (7112 kind:10002 events for a 39-author set) because a NIP-01 relay treats an overwriting REQ as a fresh EOSE replay. Leaving a resolved author in the filter a little longer is widen-safe and costs at most a few already-known deliveries. (The full reasoning lives in `sync_discovery`'s doc comment and connects to the coalescing story in *"Where did my query go?" Tracing demand through the compiler*.)

## Writes route the same way

Publishing uses the same lane facts, no relay parameter:

```swift
let intent = WriteIntent(
    pubkey: alice, createdAt: now, kind: 9999, content: "protocol payload",
    durability: .durable, routing: .authorOutbox   // ← routing CLASS, not relays
)
let receipt = try await nmp.publish(intent)
```

The current routing class selects an engine policy, not a relay set. The target
module path additionally accepts validated typed context contributions. Private
or narrow context cannot be widened by another contribution.

## Operator config is policy, not routing override

Some apps let a power user pin extra relays. That is legitimate — it enters through the `UserConfigured` lane as an additional *candidate fact* the coverage solver weighs alongside everyone's outboxes. What it can never be is a "send this specific query only here" override, because the surface to say that does not exist. The distinction is the whole point of ledger #3: config informs the compiler; it never bypasses it.

## Gaps to know

- **`ToInboxes` read-relay routing is approximate today.** Routing a write to recipients' inboxes currently falls back to the union of each recipient's write + extra relays, because `RelayDirectory` does not yet expose a dedicated per-pubkey read/inbox (kind:10050) accessor. This is flagged in `resolve_routes` and is a small additive fix to `nmp-router`, not a design change.
- **`extra_relays` on the live directory is empty** until hint/provenance ingestion is wired; today the live outbox is driven by kind:10002 writes + indexers, which is the common case.

---

<!-- nav-footer -->
<sub>← [Identity & multi-account](16-identity.md) · [Index](README.md) · [Tracing demand](18-tracing-demand.md) →</sub>
