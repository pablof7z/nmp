---
title: NIP-29 group publication — the Group door
category: nip29
slug: group-publication
status: built
date: 2026-07-29
owns:
  - the app-facing surface for reading from and writing into a NIP-29 group
  - who mints the h tag and the host route, and when
  - the pre-signed group publication path and whose requirement it is
  - where NIP-29's own kinds (9000–9022) live
  - tombstones — contextualize_group_event, GroupPublication, GroupHostAuthority
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/removed-routes.md
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
  - docs/internals/conventions/no-backwards-compatibility.md
  - docs/internals/conventions/naming-no-invented-categories.md
issues:
  - "#838 deleted group_content_demand, groupMessageIntent, and publishComposed — the precedents this design obeys"
  - "#827 folded nmp-engine into nmp; private composition layer exists"
  - "#1015 tracks the still-absent FFI/Swift Group publication projection"
  - "previous tags deliberately unimplemented; NIP-42 AUTH for private groups unverified end-to-end"
---

# NIP-29 group publication — the `Group` door

This is the settled design for how an app publishes into and reads out of a
NIP-29 relay-based group. It came out of the 2026-07-28 write-plane design
session, which also produced the universal routing design
(`docs/internals/routing/`) — group publication is deliberately a thin
consumer of that design, not a mechanism of its own.

`status: built` — §§1–9 preserve the dated pre-implementation decision record,
including its `DESIGNED` labels, rejected alternatives, and then-current source
citations. The present implementation is recorded in §10 with current-tree
anchors rather than rewriting the reasoning that selected it.

---

## 1. The ruling: `Group` is the ONLY app-facing door — DESIGNED

Pablo (repository owner, 2026-07-28):

> about nip29: sure, at the gate level, but for the app this needs to happen
> automatically; i.e. the app shouldn't say "publish to group x, relay y", it
> should create a "group" object like we've seen before and
> group.publish(event_builder_stuff) would take care of adding the h and
> publishing to the correct relay.

The app never names the host relay for a write, never spells a routing value,
and never touches the `h` tag. It holds a `Group` and hands it an event
builder:

```rust
let group = nip29::Group::new(host, "photographers");   // identity, no subscription
let sub  = engine.observe(LiveQuery(group.demand(chat_filter)), None)?;
let sub2 = engine.observe(LiveQuery(group.demand(activity_filter)), None)?; // several per room
group.publish(&engine, EventBuilder::new(Kind::from(9)).content("hello"))?;
```

`Explicit` routing stays app-constructible *in general* — publishing to a
user-typed relay and republishing a signed event to a personal archive relay
are Pablo's own first-class cases (see
`docs/internals/routing/auto-and-explicit.md`) — but the group workflow's
`Explicit` is minted by `Group`, never written by the app.

## 2. `Group::new(host, group_id)` is an identity, not a subscription — DESIGNED

The group object exists without any active subscription. This is load-bearing,
not incidental: a kind 9021 join request means writing to a group you cannot
read yet, so the write door must not require a read to exist first.

The shape is driven by consumer evidence that contradicted the first sketch.
@lark-codex's 29er-next retains `hostRelay` + `groupID` for the room lifetime
and reuses them across **several** simultaneous observations (chat 9/9000/9001,
activity 30315, reactions 7, membership/admin) plus repeated writes — so
publish cannot hang off any one query. The `Group` is the identity that mints
both read `Demand`s and writes.

## 3. There is no `group.observe` — reads use the one door — mechanism BUILT, minting DESIGNED

Pablo, checking the read model:

> is group.observe a different way of getting a LiveQuery? LiveQUery is the
> main way of getting a stream of events in nmp, right? or am I wrong?

He was right. Verified on this tree:

- `Engine::observe(query: LiveQuery, window: Option<Window>) -> Result<Subscription, EngineError>`
  (`crates/nmp/src/engine.rs:418`) is the one read door; `Subscription` is the
  stream.
- `LiveQuery(pub Demand)` is a public tuple newtype
  (`crates/nmp-resolver/src/engine.rs:149`), re-exported from the facade
  (`crates/nmp/src/lib.rs:213`) — so
  `engine.observe(LiveQuery(group.demand(filter)), None)` needs **no new
  door**.

A `group.observe()` would be a second door onto the same mechanism — exactly
the shape #838 deleted when it removed `publish_composed` as a second write
lifecycle. The `Group` therefore **mints a `Demand`**, the way
`group_discovery_demand(host)` already does today, and the app takes it
through ordinary `observe`.

The host rides on the `Demand` itself as `SourceAuthority::Pinned({host})` —
`crates/nmp-nip29/src/demand.rs:1-12` documents this as #107's primitive,
deliberately never a directory fact, so the pinned host flows through
`ContextualAtom` identity, per-source `AcquisitionEvidence`, and diagnostics
with no new mechanism.

## 4. Reads take an app-supplied kind selection — DESIGNED, with a precedent to obey

The `Group` does not decide which kinds live in the group; the app supplies the
`Filter`. This is not a stylistic choice — it is the corrected form of a
measured defect. The #838 entry in `docs/surface-change-log.md` (APPEND-ONLY;
quoted, never edited) records:

> `group_content_demand` declared `[9,30315]` to be the group's fixed content
> catalog even though NIP-29 permits foreign event kinds

(`docs/surface-change-log.md:912`. That sentence predates the terminology fix
in `docs/internals/conventions/naming-no-invented-categories.md`; as history it
stays verbatim.)

The defect was declaring a FIXED content catalog when any kind can carry an
`h` and live in a group. An app-supplied `Filter` cannot re-acquire that
defect: the crate contributes the host pinning and the `#h` scoping, the app
contributes the selection. The ownership gate
(`scripts/check-nip29-ownership.sh:41`) still bans the `group_content_demand`
identifier and the `[9,30315]` catalog by name, so any group read constructor
lands as a deliberate gate revision, not a quiet reintroduction.

## 5. Writes: the host is not derivable, so `Group` mints `Explicit` — DESIGNED

The `h` tag carries the **group id, never the relay**. The host is therefore
not derivable from the event, which means no resolver can ever compute it —
group routing is `WriteRouting::Explicit(vec![host])`, minted internally by
`Group` from the identity the app already gave it at construction. The app
never writes that value and never touches `h`.

Two consequences:

- **`nmp-nip29` needs no resolver and no dependency on `nmp`.** Verified:
  `crates/nmp-nip29/Cargo.toml` depends on exactly `nostr` + `nmp-grammar`,
  and the ownership gate (`scripts/check-nip29-ownership.sh:30-33`) fails the
  build if a core or mechanism dependency appears. The whole
  dependency-direction debate from the design session dissolved on this point:
  a crate that never computes routing from engine state has nothing to depend
  on `nmp` for.
- **`h` is appended BEFORE signing.** Pablo: "obviously it needs to have the h
  tag before its signed". Contextualization operates on the unsigned draft;
  the stamp/sign step comes after. This is already how
  `contextualize_group_event` behaves today (it takes an `UnsignedEvent`), and
  the property survives it (§9).

Note the current grammar still carries `WriteRouting::{AuthorOutbox,
PrivateNarrow, RelayListBootstrap}` (`crates/nmp-grammar/src/write.rs:207-228`)
— `Explicit` is part of the routing redesign
(`docs/internals/routing/auto-and-explicit.md`), which this door consumes.

## 6. The pre-signed path validates `h` instead of appending — DESIGNED

`group.publish_signed(signed)` (name illustrative) takes an already-signed
event and **validates** the `h` already present rather than appending one —
appending would change the bytes and therefore the `EventId`. Bytes and
`EventId` are preserved exactly; a signed event with a missing or wrong `h` is
a typed refusal.

Whose requirement this is matters for future re-litigation: it is
**@lima-codex's (Mosaico)** — their orchestration signs first, obtains the
exact `EventId`, sometimes arms an ID-correlated observation, then publishes.
Pre-signed is load-bearing there. @lark-codex's 29er-next is **100% unsigned**,
with no pre-signed call site at all. So this is one app's real requirement,
not a universal pattern — it earns a path, not the center of the design.

## 7. NIP-29's own kinds (9000–9021) belong in `nmp-nip29` — DESIGNED, IN SCOPE

Pablo, on where join/leave/moderation composition lives:

> that OBVIOUSLY belongs in the nmp-nip29 crate! nmp doesn't know about ANY of
> this, but the nip29 crate does!

Unlike kind 9 — which NIP-29 does **not** own; C7 chat is `nmp-nipc7`'s, and
the ownership gate enforces that boundary (`scripts/check-nip29-ownership.sh`)
— the 9000–9021 join/leave/moderation schema genuinely IS NIP-29's own.

**Pablo ruled these IN SCOPE for this effort — not a later addition:**

> nmp doesn't know what 'remove user from group means', but nmp-nip29 crate
> does and must provide the group.publish.... group.remove_user....
> group.join_request... -- and no, it's not "additive" in the sense that we can
> avoid shipping it during this current effort; it's IN SCOPE.

So `Group` carries typed composers for NIP-29's own operations alongside
`publish`:

```rust
group.publish(&engine, builder)?;          // any kind — kind-blind
group.join_request(&engine, invite_code)?; // 9021
group.remove_user(&engine, pubkey)?;       // 9001
group.edit_metadata(&engine, name, about)?;// 9002
```

Why this is not optional polish: without it every app looks up NIP-29's kind
numbers and tag schema itself, and a subtly wrong tag produces a relay
rejection that presents as a permissions or routing problem rather than a
malformed event. The knowledge exists in exactly one place or it is
reimplemented, differently, in every consumer.

The boundary that keeps this honest: these are the kinds NIP-29 *defines*. Kind
9 chat is NOT one of them — it is `nmp-nipc7`'s, and the ownership gate
enforces that (`scripts/check-nip29-ownership.sh`). Owning 9000–9021 does not
reopen the defect #838 closed, because that defect was NIP-29 claiming schema
belonging to others.

## 8. What the app never does — summary of the boundary

| the app… | instead |
|---|---|
| names the host relay for a write | `Group` carries it from construction |
| writes `WriteRouting::Explicit([host])` | minted internally by `Group` |
| touches the `h` tag | appended (unsigned) or validated (pre-signed) by `Group` |
| gets a group-shaped stream from a second door | `Group` mints a `Demand`; `Engine::observe` is the door |
| receives a fixed kind catalog | supplies its own `Filter` |

## 9. Tombstones — DESIGNED deletions and one abandoned design

**`contextualize_group_event` and `GroupPublication` die with this design.**
As of `b99f9d41` they still exist (`crates/nmp-nip29/src/publication.rs:17,52`,
exported at `crates/nmp-nip29/src/lib.rs:30`) and the ownership gate still
*requires* `contextualize_group_event` to be present
(`scripts/check-nip29-ownership.sh:57`). They are the build-but-cannot-deliver
half of the old world: `contextualize_group_event` returns
`GroupPublication { host, event }` and nothing in the workspace can route it.
Under this design their duties move inside `Group`, the free function and the
carrier struct are deleted in the same change (see
`docs/internals/conventions/no-backwards-compatibility.md` — no alias, no
deprecation window), and the gate is revised in that change, not evaded. What
survives them: the `h`-before-signing property, the schema-preservation
falsifier (`draft_kind_and_schema_survive_except_for_appended_h`,
`crates/nmp-nip29/src/publication.rs:98`), and the no-`previous` rule.

**`GroupHostAuthority` was designed, built uncommitted, and abandoned.** For
honesty of the record: a grammar-tier newtype
(`WriteRouting::GroupHost(GroupHostAuthority)`, mintable only from a validated
`GroupPublication`) was fully designed — including a revision of the gate's
`HostAuthority|PinnedHost` ban, which still stands today at
`scripts/check-nip29-ownership.sh:103` — on the premise that letting an app
route a write to a chosen relay was a dangerous primitive to be structurally
contained. Pablo rejected the premise outright: single-relay routing is a
first-class, general capability ("bare. It is not only overengineering; it's
wrong for many other reasons" — the full reversal is quoted in
`docs/internals/routing/auto-and-explicit.md`). With the premise dead, the
authority newtype, the dedicated route variant, and the entire "never let a
route become app-visible" line of argument died with it. The exploratory code
was never committed. NIP-29 uses the general `Explicit` route like everything
else; what remains NIP-29-shaped is only *who mints it* (§1, §5).

---

## 10. Implementation correction — BUILT (#977 / PR #1011)

PR #1011 implemented the direct-Rust `Group` door described above. The dated
`DESIGNED` labels and tombstone analysis in §§1–9 remain the decision record;
this section is the present-tense correction.

Current source anchors on this revision:

- `crates/nmp-nip29/src/group.rs:47-256` defines the `(host, group_id)`
  identity, app-selected pinned demand, unsigned `h` contextualization, and
  pre-signed context validation. It reads no kind and mints no `previous` tag.
- `crates/nmp/src/group.rs:77-256` defines and implements
  `GroupOperations`: `publish`, `publish_signed`, and NIP-29-owned
  operations all compose one ordinary `WriteIntent` and call the existing
  engine publish door. `crates/nmp/src/lib.rs:159-160` re-exports that trait.
- `crates/nmp-nip29/src/operations.rs:1-108` owns the typed 9000–9022
  join/leave/moderation builders. Kind:9 and `q` replies remain in
  `nmp-nipc7`, not this crate.
- `scripts/check-nip29-ownership.sh:65-115` bans the deleted
  `contextualize_group_event` / `GroupPublication` seam, requires both Group
  intent constructors and their schema/no-`previous` falsifiers, and requires
  the one read and one write doors.
- Native projection is still read-only:
  `crates/nmp-ffi/src/nip29.rs:1-38`,
  `Packages/NMP/Sources/NMP/NIP29.swift:1-12`, and
  `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/NIP29.kt:1-13` expose only
  `groupDiscoveryDemand`. Issue #1015 owns a future native Group publication
  door; this document claims no Swift, Kotlin, or Android write surface.

The abandoned `GroupHostAuthority` reasoning in §9 remains intentionally
visible. NIP-29 uses the general `Explicit` capability; its semantic boundary
is that `Group`, not presentation code, mints the host route and `h` context.
