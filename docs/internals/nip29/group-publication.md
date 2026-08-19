---
title: NIP-29 group publication — the Group door
category: nip29
slug: group-publication
status: built
date: 2026-07-29
owns:
  - the app-facing API for reading from and writing into a NIP-29 group
  - who mints the h tag and the host route, and when
  - why there is no pre-signed group publication path (withdrawn, #1292)
  - where NIP-29's own kinds (9000–9022) live
  - tombstones — contextualize_group_event, GroupPublication, GroupHostAuthority,
    group_discovery_demand, Group::new(host, group_id), GroupOperations,
    member_is/admin_is
related:
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolvers.md
  - docs/internals/writes/event-builder.md
  - docs/internals/writes/identity.md
issues:
  - "#838 deleted group_content_demand, groupMessageIntent, and publishComposed — the precedents this design obeys"
  - "#827 folded nmp-engine into nmp; private composition layer exists"
  - "#1108 landed the composite LiveQuery::Union a multi-host read consumes"
  - "#1033 replaced the single-host Group door with RelayScope/Group/GroupPredicate — see §11"
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
citations. §10 records PR #1011's original single-host implementation; #1033
superseded that single-host shape with a multi-relay one, recorded in §11 with
current-tree anchors. **Read §11 for the present-tense shape** — every code
example in §§1–9 and §10 illustrates the single-host `Group::new(host, id)`
door, which no longer exists (no alias); they are kept
verbatim as the reasoning record that led to the door §11 now describes, not
as usable current-day API.

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

The host rides on the `Demand` itself as `ReadRouting::Explicit({host})` —
`crates/nmp-nip29/src/demand.rs:1-12` documents this as #107's primitive,
deliberately never a directory fact, so the pinned host flows through
`ContextualAtom` identity, per-source `AcquisitionEvidence`, and diagnostics
with no new mechanism.

## 4. Reads take an app-supplied kind selection — DESIGNED, with a precedent to obey

The `Group` does not decide which kinds live in the group; the app supplies the
`Filter`. This is not a stylistic choice — it is the corrected form of a
measured defect. PR #838 deleted `group_content_demand` because it declared
`[9,30315]` to be the group's fixed content catalog even though NIP-29 permits
foreign event kinds.

The defect was declaring a FIXED content catalog when any kind can carry an
`h` and live in a group. An app-supplied `Filter` cannot re-acquire that
defect: the crate contributes the host pinning and the `#h` scoping, the app
contributes the selection. The ownership check that used to ban the
`group_content_demand` identifier and the `[9,30315]` catalog by name is
deleted along with the rest of the CI-era scripts; nothing now mechanically
prevents a quiet reintroduction of the fixed-catalog defect, so this boundary
relies on review.

## 5. Writes: the host is not derivable, so `Group` mints `Explicit` — DESIGNED

The `h` tag carries the **group id, never the relay**. The host is therefore
not derivable from the event, which means no resolver can ever compute it —
group routing is `WriteRouting::Explicit(vec![host])`, minted internally by
`Group` from the identity the app already gave it at construction. The app
never writes that value and never touches `h`.

Two consequences:

- **`nmp-nip29` needs no resolver and no dependency on `nmp`.** Verified:
  `crates/nmp-nip29/Cargo.toml` depends on exactly `nostr` + `nmp-grammar`. The
  ownership check that used to fail the build if a core or mechanism
  dependency appeared is deleted along with the rest of the CI-era scripts,
  so this boundary is currently unproven by any mechanism.
  The whole dependency-direction debate from the design session dissolved on
  this point: a crate that never computes routing from engine state has
  nothing to depend on `nmp` for.
- **`h` is appended BEFORE signing.** Pablo: "obviously it needs to have the h
  tag before its signed". Contextualization operates on the unsigned draft;
  the stamp/sign step comes after. This is already how
  `contextualize_group_event` behaves today (it takes an `UnsignedEvent`), and
  the property survives it (§9).

Note the current grammar still carries `WriteRouting::{AuthorOutbox,
PrivateNarrow, RelayListBootstrap}` (`crates/nmp-grammar/src/write.rs:207-228`)
— `Explicit` is part of the routing redesign
(`docs/internals/routing/auto-and-explicit.md`), which this door consumes.

## 6. The pre-signed path validates `h` instead of appending — WITHDRAWN (#1292)

**This path is deleted, no alias.** `Group::publish` is the group's only
write door; there is no `publish_signed`, and no API accepts bytes an app
signed itself into a group. The maintainer's ruling: apps must not sign their
own bytes and keep app-local optimistic mirrors. The legitimate need it was
built for — a signed event WITHOUT a publication — is served by
`Engine::sign_event`, which creates no write intent, pending row, receipt,
delivery lane, relay plan or publication and returns the exact signed event.
`Group::validate_context` survives as a standalone predicate over an
already-signed event; it is not a write door.

The original decision is kept below as the record of what was decided and
why it was withdrawn, not as a description of anything current.

`group.publish_signed(signed)` (name illustrative) took an already-signed
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
the ownership check that used to enforce that boundary is deleted, so it is
currently unproven by any mechanism — the 9000–9021 join/leave/moderation
schema genuinely IS NIP-29's own.

**Pablo ruled these IN SCOPE for this effort — not a later addition:**

> nmp doesn't know what 'remove user from group means', but nmp-nip29 crate
> does and must provide the group.publish.... group.remove_users....
> group.join_request... -- and no, it's not "additive" in the sense that we can
> avoid shipping it during this current effort; it's IN SCOPE.

So `Group` carries typed composers for NIP-29's own operations alongside
`publish`:

```rust
group.publish(&engine, author, builder)?;                  // any kind — kind-blind
group.join_request(&engine, author, invite_code)?;         // 9021
group.add_users(&engine, author, users)?;                  // one 9000
group.remove_users(&engine, author, pubkeys)?;             // one 9001
group.edit_metadata(&engine, author, metadata_edit)?;      // 9002
```

`add_users` and `remove_users` take the whole moderation change. NIP-29
permits several `p` rows in one kind:9000/9001 event, so the facade publishes
one signed event and returns one ordinary receipt. It never loops one write per
pubkey. Exact duplicates collapse in pubkey order; an empty operation or one
pubkey assigned conflicting roles is refused before any write is accepted.

Why this is not optional polish: without it every app looks up NIP-29's kind
numbers and tag schema itself, and a subtly wrong tag produces a relay
rejection that presents as a permissions or routing problem rather than a
malformed event. The knowledge exists in exactly one place or it is
reimplemented, differently, in every consumer.

The boundary that keeps this honest: these are the kinds NIP-29 *defines*. Kind
9 chat is NOT one of them — it is `nmp-nipc7`'s, and the ownership check that
used to enforce that is deleted, so this boundary is currently unproven by
any mechanism. Owning 9000–9021 does not
reopen the defect #838 closed, because that defect was NIP-29 claiming schema
belonging to others.

## 8. What the app never does — summary of the boundary

| the app… | instead |
|---|---|
| names the host relay for a write | `Group` carries it from construction |
| writes `WriteRouting::Explicit([host])` | minted internally by `Group` |
| touches the `h` tag | appended by `Group` before signing |
| gets a group-shaped stream from a second door | `Group` mints a `Demand`; `Engine::observe` is the door |
| receives a fixed kind catalog | supplies its own `Filter` |

## 9. Tombstones — DESIGNED deletions and one abandoned design

**`contextualize_group_event` and `GroupPublication` die with this design.**
As of `b99f9d41` they still exist (`crates/nmp-nip29/src/publication.rs:17,52`,
exported at `crates/nmp-nip29/src/lib.rs:30`) and the ownership check still
*required* `contextualize_group_event` to be present. They are the build-but-cannot-deliver
half of the old world: `contextualize_group_event` returns
`GroupPublication { host, event }` and nothing in the workspace can route it.
Under this design their duties move inside `Group`, the free function and the
carrier struct are deleted in the same change (no alias, no deprecation
window), and the gate is revised in that change, not evaded. What
survives them: the `h`-before-signing property, the schema-preservation
falsifier (`draft_kind_and_schema_survive_except_for_appended_h`,
`crates/nmp-nip29/src/publication.rs:98`), and the no-`previous` rule.

**`GroupHostAuthority` was designed, built uncommitted, and abandoned.** For
honesty of the record: a grammar-tier newtype
(`WriteRouting::GroupHost(GroupHostAuthority)`, mintable only from a validated
`GroupPublication`) was fully designed — including a revision of the
ownership check's `HostAuthority|PinnedHost` ban, which stood until that
check was deleted along with the rest of the CI-era scripts — on the premise
that letting an app
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

## 10. Implementation correction — SUPERSEDED (#977 / PR #1011)

**Superseded by §11.** PR #1011 implemented the direct-Rust `Group` door
described above, single-host. #1033 deleted every symbol this section names
(`crates/nmp-nip29/src/group.rs`, `crates/nmp/src/group.rs`, the
`GroupOperations` trait, `groupDiscoveryDemand`) with no alias, replacing them
with the multi-relay `RelayScope`/`Group`/`GroupPredicate` shape in §11. This
section is kept as the historical record of what PR #1011 actually shipped,
the same way §§1–9 are kept as the decision record — do not treat any path or
signature below as current.

Source anchors as PR #1011 left them — historical, not current (see §11):

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
- The (now-deleted) ownership check banned the deleted
  `contextualize_group_event` / `GroupPublication` seam, required both Group
  intent constructors and their schema/no-`previous` falsifiers, and required
  the one read and one write doors.

---

## 11. Multi-relay correction — BUILT (#1033)

A group can live on more than one relay at once. `Group::new(host, group_id)`
pinned exactly one; #1033 replaces it with a scope an app names once and
narrows, with no single-host convenience overload left behind (a singleton is
`nip29::on([host])?.group(id)`). This section is the present-tense
description; §§1–10 are the decision/implementation record that led here.

**Two nouns, one narrowing.**

```rust
let relays = nip29::on([relay_a, relay_b])?;              // RelayScopeError::EmptyRelaySet if empty
let group  = relays.group("photographers");                // same hosts, one group id

relays.observe(&engine, mine, [Metadata, Admins, Members], None)?; // who is in which group
relays.observe(&engine, nip29::all(), [Metadata], Some(250))?;    // what this relay advertises
engine.observe(group.read(chat_filter)?, None)?;                  // this group's content
group.publish(&engine, author, EventBuilder::new(Kind::from(9)).content("hi"))?;
```

`nmp::nip29::on` is fallible where the deleted single-host door was not: a
one-element pinned set can never be empty, but a caller-supplied SET can.
`RelayScope`/`Group` are opaque, retain their hosts/id privately, and expose
no accessor for either — an app cannot compose an event under one scope and
route it under another because there is no spelling for saying so.

**Per-relay authority, not per-group.** The `h` tag is a label; the relay
decides. Two relays hosting the same group id are two independent groups with
the same name — membership and the relay-signed 39000/39001/39002 metadata
both diverge. NMP surfaces that divergence (the addressable coordinate
includes the author pubkey, so two relays' own 39000s never compete); it does
not collapse it. §5's "the host is not derivable, so `Group` mints `Explicit`"
still holds, widened to the whole set: every group write routes
`WriteRouting::Explicit(all scope hosts)`, never one host, never a fallback.

**Reads are one ordinary `LiveQuery`, never a per-host list the app merges.**
One host yields `LiveQuery::Single`; more than one yields the `LiveQuery::Union`
of complete singleton-host branches that #1108 added for exactly this
consumer. Every NIP-29-owned nesting level inside a branch — the outer listing
demand and any inner evidence lookup a discovery predicate builds — is pinned
to that one branch's host, stamped explicitly rather than inherited, because
resolving evidence at relay A while listing at relay B would be a confidently
*wrong* answer, not a slow one. A caller-owned inner binding (e.g. a kind:3
follows lookup) keeps its own authority; NIP-29 never recursively repins it.

**Cache is scoped to the host too, not just the wire request.**
`ReadRouting::Explicit` alone only scopes which relay is *asked*;
`CacheMode` separately governs which already-cached rows may *answer*, and the
grammar's `Agnostic` default ignores provenance — so a naive pinned demand
could let host A's cached kind:39002 row answer host B's structurally
identical lookup, reporting a member nothing at B actually supports. Every
NIP-29-owned demand sets `CacheMode::Strict` at the one choke point
(`explicit_at`) every constructor passes through, closing that leak. The
user-visible consequence: a just-published group message appears under a
host once *that host* has ACKed it, not immediately under every host in the
scope — showing an event under a host that rejected it would be exactly the
wrong answer this door exists to prevent. Do not describe cross-host
appearance as immediate or synchronized; it is per-host, on that host's own
acceptance.

**Discovery is evidence-scoped, not exact-state.** kind:39002 (members) and
kind:39001 (admins) are optional, possibly-partial relay-signed lists:
inclusion is evidence, absence is not evidence of the opposite. The API is
therefore `nip29::member_list_includes(subjects)` /
`nip29::admin_list_includes(subjects)`, returning a composable
`GroupPredicate` (`union`/`intersect`/`minus`, folding with the grammar's own
`SetOp`) — never `member_is`/`admin_is`, which would claim exact current
state the underlying kinds cannot establish.

**Current source anchors:**

- `crates/nmp-nip29/src/context.rs` — the `h` rows: `contextualize`,
  `validate_context`, `group_demand_at` (one host's complete read branch for
  one group id). Both write-side functions take a nonempty SET of group ids
  (#1281): `contextualize` appends one `h` per id in the set's canonical
  order and `validate_context` requires an already-signed event's `h` rows to
  name exactly that set, each once. One group is the one-element case, with
  no separate path. Kind-blind; mints no `previous` tag. Carries the schema- and
  no-`previous`-preservation falsifiers
  (`draft_kind_and_schema_survive_except_for_appended_h`,
  `publication_never_synthesizes_previous`).
- `crates/nmp-nip29/src/discovery.rs` — `GROUP_METADATA_KIND` /
  `GROUP_ADMINS_KIND` / `GROUP_MEMBERS_KIND` (39000/39001/39002),
  `member_list_includes_at`, `admin_list_includes_at`: one host's complete
  discovery branch, every NIP-29-owned nesting level pinned to that host. `explicit_at` is the one choke point every NIP-29 demand
  passes through for BOTH axes: `ReadRouting::Explicit` (which relay is
  asked) and `CacheMode::Strict` (which cached rows may answer) — closing the
  cross-host cache leak a merely-pinned-but-`Agnostic` demand would otherwise
  have (`0ec66f8d`).
- `crates/nmp-nip29/src/records.rs` (#1233) — what those three records SAY:
  `GroupRecord`, `GroupMetadata`, `ListedRecord`, `ListedSubject`,
  `group_metadata_at`, `listed_record_at`, `join_key_of`, and
  `group_records_at(host, records, predicate)` — one host's branch for the
  records the app actually named. The per-event projection lives here, beside
  the schema it parses, because a crate that owns a schema and does not own
  the only correct way to read it produces divergent hand-rolled parsers, which
  is exactly what #1233 measured.
- `crates/nmp-nip29/src/operations.rs` — the typed 9000–9022 composers,
  unchanged in shape by this issue.
- `crates/nmp/src/nip29/mod.rs` — `nip29::on`, `RelayScope`,
  `RelayScope::group`, `RelayScope::groups`, `RelayScope::observe`, the `nip29::group(hosts, id)`
  sugar, and the falsifier that
  exists to prove per-host stamping survives nesting,
  `scope_stamps_exact_hosts_on_every_nested_nip29_demand`. `nmp::nip29` is a
  real module here (`crates/nmp/src/lib.rs:160`), not a re-export of
  `nmp-nip29` — the door needs both the retained scope and the one opaque
  `WriteIntent`, and the engine-free lower crate cannot mint the latter.
- `crates/nmp/src/nip29/groups.rs` (#1281/#1283) — `Groups`, the WRITE
  CONTEXT: the scope's hosts plus the set of ids one event claims, with
  `contextualize`, `validate_context`, `intent`, `signed_intent`, `publish`
  and `publish_signed`. Write-only by design — a read, a roster and every
  9000-9022 action are per-group by definition, so a plural of any of them
  would be an aggregate this crate would have to invent a meaning for.
  `Group`'s whole write half delegates to a private one-element `Groups`, so
  "one group is the one-element case" is a property of the code
  (`a_group_write_is_the_one_element_case_of_a_several_group_write`).
- `crates/nmp/src/nip29/group.rs` — `Group`'s inherent `read`,
  `validate_context`, `publish` (the ONE write door since #1292 deleted
  `intent`/`signed_intent`/`publish_signed`), and the named operations
  (`join_request`, `leave_request`, `add_users`, `remove_users`,
  `edit_metadata`, `delete_event`, `create_group`, `delete_group`,
  `create_invite`) — all inherent, no `GroupOperations` trait. Carries
  `a_group_write_routes_explicitly_to_every_host_in_the_scope`.
- `crates/nmp/src/nip29/predicate.rs` — `GroupPredicate`,
  `member_list_includes`, `admin_list_includes`, `union`/`intersect`/`minus`.
- `crates/nmp/src/nip29/read.rs` — folds one branch per host into the one
  `LiveQuery` (`LiveQuery::single`/`LiveQuery::union`), consuming #1108.
The ownership check that used to be retargeted at this shape — requiring
`RelayScope`, the evidence-scoped predicates, both falsifiers by name, and
forbidding `crates/nmp-nip29/src/{group,demand}.rs`, `crates/nmp/src/group.rs`,
the `GroupOperations` trait, and a lower `WriteIntent` reference from
reappearing — is deleted along with the rest of the CI-era scripts; this
shape is currently unproven by any mechanism.

Deleted, no alias: `group_discovery_demand` and its `pinned_demand` helper;
`Group::new(host, group_id)` and every single-host constructor; `Group::demand`
and `Group::write_intent`/`signed_write_intent` on the lower crate (the intent
factories moved to the facade `Group` above); the `GroupOperations` extension
trait; and the overclaiming `member_is`/`admin_is` spellings.

The abandoned `GroupHostAuthority` reasoning in §9 remains intentionally
visible. NIP-29 uses the general `Explicit` capability; its semantic boundary
is that `Group`, not presentation code, mints the host route and `h` context.
