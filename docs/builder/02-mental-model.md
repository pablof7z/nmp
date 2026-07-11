# The mental model

> The behavior in this chapter is the provisional v2 contract. Public spelling
> is illustrative; [current implementation status](03-status-map.md) is tracked
> separately.

The whole engine fits in one sentence:

> **Closed values go in; snapshots and receipts come out; app code runs after
> delivery.**

## One picture

```text
YOUR APP                         NMP ENGINE                     YOUR APP

Demand {                    canonical store               QuerySnapshot {
  selection,     ------->   dependency graph   ------->     rows,
  source,                    relay compiler                  cacheEvidence,
  access                     sync + dedup                    acquisition,
}                            limits                         shortfall
                                                            }

WriteIntent {               acceptance journal           Receipt facts:
  immutable draft, -------> pending row        ------->   accepted
  durability,               signer orchestration           awaitingSigner
  context,                  routing + retry                 signed
  signer override?                                           sent/acked/...
}

currentPubkey -----------> reactive input + default signer selection
capabilities -----------> signer / AUTH / crypto plug points
diagnostics <----------- read-only explanation of the same engine state
```

NMP never owns the boxes on the far right. Your app folds snapshots and
receipts into whatever state model it already uses, then orders, formats, and
renders them.

## Noun one: a live query

A live query is a semantic demand:

```text
Demand = Selection + SourceAuthority + AccessContext
```

- **Selection** is a Nostr filter whose set-valued fields may contain closed
  reactive bindings.
- **Source authority** says which typed routing facts may acquire the rows.
- **Access context** captures facts that can change a source's answer, such as
  an AUTH identity or a protocol visibility grant.

The engine can hash, compare, print, route, coalesce, and re-root that value.
Equal selections may share local matching; source/access evidence is shared
only when the full relevant context is equal.

An observer receives the current canonical local rows plus compact evidence
about the cache and the sources NMP actually planned. It does not receive a
claim about the complete global Nostr result.

## Noun two: a write intent

A write intent contains an immutable unsigned draft, a durability policy,
typed protocol/routing context, and an optional signer-identity override.

For a durable intent, `Accepted` means one atomic persistence boundary owns:

- the frozen event body and final event id;
- the expected signer identity;
- the receipt and delivery obligation; and
- one canonical pending row in the ordinary event store.

Matching queries see that pending row through normal store invalidation. The
write path does not call observers or maintain a separate optimistic overlay.
The signature later promotes the same row without changing its id.

An explicitly non-durable intent still returns observable status. Its receipt
states the weaker retention promise; it is not a silent `void` publish.

## Current pubkey is an input, not a session

`$currentPubkey` has two ergonomic roles:

1. query bindings may react to it; and
2. its registered signer is the default for a write.

Changing it re-roots only demand that references it. Literal multi-account
queries stay live. An already accepted write keeps the signer identity chosen
at acceptance. A write may name a different signer without making that identity
current.

The app owns its account list, labels, selection UI, import/export, and logout
policy. One engine instance has one shared cache trust domain. A mutually
untrusted logout uses an explicit destructive reset rather than pretending an
account switch partitions cached rows.

## Why engine inputs are values

NMP must understand anything that changes demand, routing, source access,
signer choice, persistence, or admission. A closure in that path cannot be
hashed, deduplicated, serialized, compared across platforms, or explained in
diagnostics.

The boundary is therefore:

- **before delivery:** closed values;
- **after delivery:** arbitrary app code.

Custom ranking, formatting, moderation, grouping, navigation, and view state
belong after delivery. They can be as application-specific as needed.

## Protocol meaning is opt-in

Core knows universal Nostr machinery, not a preferred content model. An opt-in
protocol module may own exact event schemas, validation, state reconstruction,
closed query helpers, semantic write operations, and typed source/routing
context defined by that protocol.

Composition stays immutable. A NIP-29 group can add its `h` tag and host relay
context to a NIP-68 photo draft without owning the photo kind. Core freezes the
final draft, selects one signer, signs once, maintains one canonical row, and
publishes one intent.

The raw two-noun engine remains useful with no protocol module enabled.

## Diagnostics is proof, not a third command surface

Diagnostics shows the graph expansion, source plan, exact wire filters,
connections, AUTH, EOSE/watermarks, received counts, limits, retry facts, and
shortfall that produced ordinary snapshots and receipts.

It is permanent and read-only. Observing diagnostics cannot change demand or
route work.

---

<sub>[Index](README.md) · Related: [Why NMP exists](01-why-nmp.md) · [Ten-minute embedding](04-ten-minute-timeline.md)</sub>
