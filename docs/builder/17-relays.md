# Relays: outbox, indexers, and roles — you never pick relays

**Status: BUILT** — the coverage router, lane vocabulary, and the self-bootstrapping outbox are live and headlessly proven end-to-end (`nmp-engine/tests/self_bootstrap_outbox.rs`, `nmp-router/src/{route,solver,facts}.rs`).

After this chapter you will understand why there is no `relays:` parameter anywhere in NMP, how one relay can play several roles at once, and how the engine navigates the entire outbox model from just two indexer relays you configure at startup.

## There is no `relays:` parameter

Search the public surface for a way to say "fetch this from wss://relay.example.com." There isn't one. `observe(_ filter:)` takes a filter; `publish(_ intent:)` takes an intent; the Rust `Handle` has exactly five verbs plus `shutdown`, and none of them accepts a relay list. This is **bug-class ledger #3 (wrong-relay routing / manual relay lists)** as a property of the types: relay choice is *compiler output*, not caller input. You declare *what* you want; the engine decides *where* to get it.

The only relay fact you ever supply is a set of **indexer relays** at construction:

```swift
let nmp = try NMPEngine(config: .init(
    storePath: cachePath,
    indexerRelays: ["wss://relay.damus.io", "wss://purplepag.es"]
))
```

Two is plenty. From here the engine bootstraps every other relay decision itself. Note what `indexerRelays` is *not*: it is not "the relays your content comes from." It is a discovery starting point — the seed from which the engine learns everyone's actual outboxes.

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

Two things to notice. First, `UserConfigured` is a lane, not an escape hatch: an operator can add relays, but they enter as *policy facts the compiler weighs*, never as a "send this query here" override. Second, `IndexerDiscovery` is fenced — an indexer is eligible only for discovery-kind atoms, and is *never* a content fallback. If an author's content relay is unknown, their notes route nowhere until the engine discovers the real one; the engine never dumps content REQs onto your indexers.

## Roles are additive

The single most important thing to internalize: **a relay's roles stack.** A relay is not "an indexer" *or* "a write relay." The same URL can be both, and when it is, it does both jobs.

Discovery kinds are `{0, 3}` plus the whole NIP-01 replaceable range `10000..=19999` (`DiscoveryKinds::default`). An atom whose kinds are all discovery kinds *may* use the `IndexerDiscovery` lane. A content atom (kind:1, say) never may. But the two eligibilities are computed **independently and unioned**:

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
- A's kind:1 content atom *also* routes there — via the `Nip65Write` lane, because it is genuinely A's outbox.

The relay carries `0/3/1xxxx` because it is an indexer, and it *additionally* carries A's content because it is in A's mailbox. Nothing about being an indexer excludes it from also being a content relay. The router's own `additive_relay_role_*` tests pin this exact property.

## Self-bootstrapping: two indexers become the whole outbox graph

Here is the mechanism that lets you configure two relays and never think about relays again. At startup the engine's relay directory (`LiveDirectory`) knows *only* your indexers. Every author's write relays start empty. The engine fills them in over time, entirely on its own.

The internal loop (`EngineCore::sync_discovery`, run on every recompile):

1. Look at current demand. For every author referenced whose write relays are still unknown, the engine opens an **internal kind:10002 discovery subscription** covering exactly those authors.
2. That discovery atom (`kinds:[10002], authors:{unknown}`) is just another entry in the demand set — it is a discovery kind, so the *existing* router eligibility routes it to your indexers. No special path.
3. When an author's kind:10002 arrives, the engine re-reads the store's current winner for it and feeds the write relays into the directory (`ingest_write_relays`).
4. The very next recompile routes that author's *content* atoms to their now-known write relays — never to the indexer.

The headline falsifier test walks this exactly. Configure a single indexer, follow author A, then:

- **Before A's kind:10002 arrives:** the engine has already opened its own kind:10002 discovery REQ for A against the indexer, and A's kind:1 content atom is routed **nowhere** — the test asserts it is *not* on the indexer, because indexers are never a content fallback.
- **After A's kind:10002 lands** (declaring write relay R): A's kind:1 atom re-routes to **R**, and the test asserts it is on R and still not on the indexer.

That is the whole outbox model, self-navigated from your two seed relays. You declared "I follow A and want A's notes"; the engine discovered where A actually writes and went there.

The discovery subscription is deliberately **widen-only**: once an author is in it, they stay in it even after resolving, and it only tears down when *no* author needs discovering. This is not sloppiness — reopening a narrower subscription on every author-resolves-one-at-a-time event caused a real triangular-number over-fetch (7112 kind:10002 events for a 39-author set) because a NIP-01 relay treats an overwriting REQ as a fresh EOSE replay. Leaving a resolved author in the filter a little longer is widen-safe and costs at most a few already-known deliveries. (The full reasoning lives in `sync_discovery`'s doc comment and connects to the coalescing story in *"Where did my query go?" Tracing demand through the compiler*.)

## Writes route the same way

Publishing uses the same lane facts, no relay parameter:

```swift
let intent = WriteIntent(
    pubkey: alice, createdAt: now, kind: 1, content: "gm",
    durability: .durable, routing: .authorOutbox   // ← routing CLASS, not relays
)
let receipt = try await nmp.publish(intent)
```

`routing: .authorOutbox` means "fan out to my write relays" — the engine resolves those from the same kind:10002 facts the read path uses. You choose a routing *class* (`authorOutbox` / `toInboxes` / `privateNarrow`), never a relay set. Private routing is fail-closed and covered in *Provenance, and why private events can't be republished*.

## Operator config is policy, not routing override

Some apps let a power user pin extra relays. That is legitimate — it enters through the `UserConfigured` lane as an additional *candidate fact* the coverage solver weighs alongside everyone's outboxes. What it can never be is a "send this specific query only here" override, because the surface to say that does not exist. The distinction is the whole point of ledger #3: config informs the compiler; it never bypasses it.

## Gaps to know

- **`ToInboxes` read-relay routing is approximate today.** Routing a write to recipients' inboxes currently falls back to the union of each recipient's write + extra relays, because `RelayDirectory` does not yet expose a dedicated per-pubkey read/inbox (kind:10050) accessor. This is flagged in `resolve_routes` and is a small additive fix to `nmp-router`, not a design change.
- **`extra_relays` on the live directory is empty** until hint/provenance ingestion is wired; today the live outbox is driven by kind:10002 writes + indexers, which is the common case.

---

<!-- nav-footer -->
<sub>← [Identity & multi-account](16-identity.md) · [Index](README.md) · [Tracing demand](18-tracing-demand.md) →</sub>
