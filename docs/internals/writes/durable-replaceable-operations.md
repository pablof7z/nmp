---
title: Durable operations for replaceable events
category: writes
slug: durable-replaceable-operations
status: designed
date: 2026-08-13
owns:
  - the behavioral difference between an app operation and one materialized Nostr event
  - offline and multi-device replay of operations over source-qualified replaceable state
  - receipt, event-id, signature, routing, and relay-evidence truth across rematerialization
  - capability ownership of encoding, conflict, private-content, and migration policy
  - the body-complete custody boundary for configured capability materializers
related:
  - docs/design/durable-write-signing-and-retry.md
  - docs/internals/writes/payload-and-replaceable-edits.md
  - docs/internals/writes/publish-queue.md
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/known-gaps.md
issues:
  - "#1380 — offline-safe semantic operations epic"
  - "#1382 — encrypted candidate preparation before custody"
  - "#1412 — rejected bodyless materializer experiment"
  - "#1414 — this behavior specification"
  - "#1432 — body-complete semantic operation acceptance"
  - "#1433 — complete successor rematerialization"
  - "#1434 — generation-qualified signing and delivery"
---

# Durable operations for replaceable events

This document explains a target behavior. It is written for someone who knows
Nostr deeply but does not know NMP's internal architecture.

Most of the target behavior in this document is **not built**. NMP now accepts
configured semantic operations only after producing a complete optimistic
event, retains their ordered operation meaning under ordinary receipts, and
atomically replays active operations over each newer verified relay source.
That source and its complete successor commit together, so a live query never
exposes the raw source between complete local generations. Generation-qualified
signing and relay publication remain owned by #1434.

That distinction matters when reading this document:

- **Today** means current, tested NMP behavior on `master`.
- **Target** means the behavior this design requires before the work may be
  promoted as complete.
- **Candidate architecture** means a promising implementation direction. It
  is intentionally separated near the end because it is more likely to change
  than the behavioral contract.
- **Open decision** means the design must still choose a policy. The document
  does not quietly turn an unanswered question into an implementation claim.

The stable center of this design is simple:

> A user's action on replaceable state is not necessarily one Nostr event.
> The action must remain durable while NMP produces, signs, and delivers the
> exact event or sequence of successor events needed to carry it out.

---

## 1. The problem in Nostr terms

Every Nostr event is immutable. A regular event such as kind `1` remains an
independent stored assertion rather than competing for one NIP-01 replacement
winner. If an app asks to publish it, the event body can be fixed once. A relay
later receiving another kind `1` does not change what the first event meant.

A replaceable event is different. [NIP-01](https://github.com/nostr-protocol/nips/blob/master/01.md)
defines kinds `0`, `3`, and `10000...19999` as replacing
another event at `(author, kind)`. Kinds `30000...39999` replace another event
at `(author, kind, d)`. The current value is selected by NIP-01 ordering:
greater `created_at` wins and the lexicographically smaller event id breaks an
equal-timestamp tie.

Suppose a user changes only their profile name while offline. The device can
construct a complete kind `0` from the profile it currently knows. But while
it is offline, another device may change the profile picture. Publishing the
first device's frozen whole event later can make the name change succeed by
erasing the newer picture change.

The durable fact the first device should retain is therefore not necessarily:

```text
publish these exact kind-0 bytes
```

It may instead be:

```text
set the name field to "Pablo" on the best qualified profile state
```

That operation can be replayed over a newer profile event. The profile module
knows how profile fields map to JSON and which unknown or malformed content it
must preserve. Generic publishing infrastructure does not.

This is not special to profiles or NIP-02. The same distinction appears in:

- a follow or unfollow operation represented by a kind `3`;
- adding one relay to a relay list without overwriting a concurrent change;
- changing a long-form article title while preserving a body edit from
  another device;
- adding or removing one item in an addressable public or private list;
- applying a protocol-specific migration while preserving fields the current
  implementation does not understand.

The requirement applies to ordinary replaceable and parameterized addressable
events. Kind `3` is one useful example, not the architecture's privileged
case.

### 1.1 Why exact whole-event replacement remains useful

Not every schema has a meaningful or implemented semantic operation. An app
may already possess the complete desired next event and deliberately want an
all-or-nothing compare-and-swap against the event it read.

That remains a separate, valid behavior:

```text
replace source E7 with this complete body, but only if E7 is still current
```

If E8 has become current, NMP refuses. It must not guess how to merge a body
whose semantics it does not own.

Semantic replay extends the write model; it does not turn every whole-event
replacement into a guessed field merge.

### 1.2 Why “online” is the wrong abstraction

A Nostr client is not simply online or offline. At one moment it may:

- have a signer but no relay connection;
- be connected to one of an author's three write relays;
- have reconciled two selected sources and still be waiting on a third;
- know four recipients' inbox relays and still be resolving a fifth;
- have a complete unsigned event but be waiting on a remote signer;
- have one relay accept the current event while another is in backoff;
- discover a newer source event after a predecessor was already accepted by a
  relay.

The design therefore speaks about exact knowledge and work: which source has
been checked, which event is currently materialized, which signer result
belongs to it, and which relay has answered for which event id.

---

## 2. Vocabulary: five identities that must not collapse

The following concepts are deliberately distinct. Most subtle failures in
this problem come from treating two of them as one.

Two additional terms recur throughout:

- A **capability** is the protocol-specific module that knows one resource's
  meaning and exposes typed actions such as `set_name` or `follow`. It is not a
  signer permission or a relay feature in this document.
- An **operation program** is the compact, durable representation of the
  still-contributing capability actions for one resource. It can be replayed
  after restart; the app never constructs or interprets it directly.

### 2.1 App action

The user-level change the app requested: “publish this note,” “set my profile
name,” “follow Alice,” or “change this article's title.” A capability-specific
API may collect several changes before one save, such as:

```text
profile.set_name("Pablo")
profile.set_nip05("_@f7z.io")
profile.save()
```

The app action describes meaning. It need not contain a complete Nostr event.

### 2.2 Receipt and accepted operation

Every write request that enters NMP's receipt custody gets a **receipt id**: a
durable evidence identity returned to the app. It is not a Nostr relay message
and it is not an event id.

The receipt may move immediately to terminal `Refused` before NMP accepts a
publication obligation—for example, when an exact replacement's base has
changed. **Accepted** is the distinct transition where NMP takes durable
responsibility for carrying out the operation. After that transition, a
semantic receipt identifies one accepted app operation for its lifetime.

Two accepted calls return two receipt ids even when their operations are
compacted into one event. The reason is behavioral, not bookkeeping: the app
may independently ask what happened to each action, cancel one eligible action,
or correlate each call with its own UI or business record.

### 2.3 Materialization

A **materialization** is one complete NIP-01 event body produced while carrying
out one or more accepted operations. Before signing it has an author, kind,
tags, content, `created_at`, and event id, but no valid signature. After signing
it is a complete wire event.

One receipt may contribute to several successive materializations:

```text
receipt R1 → materialization E1 → newer source found → materialization E2
```

Several receipts may contribute to one materialization:

```text
follow Alice R1 ─┐
follow Bob   R2 ─┼→ one kind-3 event E3
follow Carol R3 ─┘
```

An event id always identifies the exact serialized NIP-01 body of one
materialization—the author, timestamp, kind, tags, and content. It does not
commit the signature field. It must never be used as a permanent alias for the
receipt.

### 2.4 Source-qualified current event

A **source** is a place the chosen read policy consults for replaceable state.
A **source-qualified current event** is the current replacement winner observed
under that explicit source plan and access context.

A **source plan** is that explicit bounded read policy: which relays and access
identities count, what evidence is required from them, and when the plan is
**closed**. Closed means the policy expects no more source work for this
operation; it does not mean the network is globally complete.

An **access context** is the relay-facing identity under which a query or write
occurs—for example, unauthenticated public access or NIP-42 authentication as
pubkey P. Evidence and relay responses obtained under different access
contexts are not interchangeable merely because the relay URL is the same.

A **source revision** is the exact source-evidence snapshot used for one
materialization: the selected event id or qualified absence, plus enough plan
identity to reject a result computed after that evidence changes. It is not a
server-assigned version number.

It is not a claim that the event is the global Nostr head. Nostr has no global
head. It means only that the event is current among the evidence the selected
plan has obtained.

NMP also maintains an **effective local view**: the value applications should
currently see after applying accepted local operations to the best qualified
source. The source event and the effective local event must remain separately
observable. Otherwise an optimistic local result can masquerade as something
a relay actually supplied.

### 2.5 Per-event, per-relay delivery evidence

Delivery evidence says what happened when one exact signed event was offered
to one exact relay session. It must include the event id.

Examples:

```text
relay A accepted event E1
relay B rejected event E1 as blocked
relay C is waiting to retry event E2
```

“Relay A accepted E1” says nothing about E2, even if E2 is the successor for
the same receipt. It also says nothing about what any other relay considers
current.

---

## 3. Three distinct publication modes

The public model remains one write request and one receipt lifecycle, but that
write request can express three fundamentally different replaceability
commitments. An externally pre-signed event is orthogonal: its complete body,
event id, author, and signature are all fixed and verified verbatim, so it
follows the fixed-event behavior whether its kind is regular or replaceable.

### 3.1 Ordinary event: fixed bytes after acceptance

The app supplies a complete event body to be authored by the selected identity.
NMP freezes the body and timestamp, derives the event id, obtains a signature,
routes the event, and retries those exact bytes.

There is no base event and no rematerialization. A kind `1` is the clearest
example.

The generic fixed-body door can also carry a replaceable kind. That is an
unconditional publication of exactly the supplied value: it carries no source
qualification, preservation proof, exact-base guard, or replay meaning. It is
useful for importing or deliberately publishing a complete event, but it must
not be presented as a safe semantic edit. Capability APIs that promise safe
editing use the exact or replayable modes instead.

**Open surface decision:** the cleaner target may structurally refuse an
unsigned replaceable/addressable `Event(builder)` and require it to use either
a capability-owned exact workflow—including qualified first-value creation—or
a replayable operation. Externally pre-signed replaceable events must remain
publishable verbatim because NMP cannot alter their bytes. The current generic
door allows the unsigned blind-replacement escape hatch; retaining versus
deleting it must be decided before the final payload cut.

### 3.2 Exact replacement: capability-owned body plus exact base precondition

A capability workflow supplies the complete desired replacement body and the
exact current event id it used as a base. Acceptance compares that id with the
current local winner at the resolved author's coordinate as one
crash-consistent acceptance transition.

A bare event id proves only the compare-and-swap condition. It does not prove
that the caller used qualified sources, preserved unknown fields, or understood
the schema. The raw expected-base guard is therefore not application-mintable
authority in the target surface. Apps reach it through the typed capability
workflow that owns those policies.

If the base still matches, NMP accepts and publishes the complete replacement.
If it does not, NMP returns a durable typed refusal. It does not silently
rebase, because it lacks a semantic operation that could be replayed safely.

### 3.3 Replayable operation: meaning plus a materialization owner

The app supplies a typed capability operation. The capability converts it into
a bounded, versioned durable replay representation. NMP retains that
representation and later evaluates it, together with the selected source event
and a target timestamp, through the replay mechanism selected by the
architecture.

Successful replay produces a complete unsigned event body. NMP validates that
it targets the requested coordinate, derives the event id, obtains the
signature, and uses the ordinary routing and delivery machinery.

If the qualified source changes while the obligation is active, NMP evaluates
the still-contributing operation program over that source. The resulting
successor is a new event with a new event id under the same operation receipts.

These modes must be mutually exclusive. A write must not carry both a complete
replacement body and a semantic operation as two competing authorities.

---

## 4. Worked examples

The examples describe behavior, not final Rust or Swift spelling.

### 4.1 Non-replaceable kind `1`: publish one note

The app asks:

```text
publish kind 1 with content "Hello"
```

NMP accepts one complete body, freezes one event id, and obtains one signature.
All relay attempts concern the same event id. A later kind `1` is a separate
event and cannot cause this receipt to rematerialize.

If the signer is unavailable, the complete body may already appear in matching
local live queries as signature-pending. It is a complete event body with a
typed “signature pending” property. It does not contain an empty signature or
an all-zero signature presented as real data.

This ordinary case is important because semantic replaceable operations must
not make the simple path pay for source lookup or semantic replay.

### 4.2 Kind `10002`: capability-owned exact whole-event replacement

Assume the app has current relay-list event E7 and wants the complete next list:

```text
wss://a.example  read + write
wss://b.example  read
```

The relay-list capability submits that whole body with `expected base = E7`.

- If E7 is still current at the author's kind-10002 coordinate, NMP accepts it.
- If E8 is now current, NMP refuses the write with expected E7 and actual E8.
- The refusal has its own receipt evidence, but it creates no optimistic event,
  signer work, route, or delivery work.
- NMP does not infer whether the app meant to add relay A, remove relay C, or
  replace the entire list. The request did not carry that meaning.

This exact fallback is appropriate for schemas without a replayable operation
or for an app that deliberately wants whole-value compare-and-swap behavior.

Current NMP freezes the rejected draft far enough to report its candidate event
id alongside the receipt, expected base, and actual base. That candidate never
becomes an accepted event row or an open publication obligation.

An omitted expected base means “this scoped local view currently has no
winner.” It never asserts that no event exists anywhere on Nostr. A later
source observation can still reveal one. If it arrives before the
crash-consistent acceptance boundary, the exact replacement is terminally
refused because the expected absence no longer matches. If it arrives after
acceptance, the accepted request remains a frozen fixed-body publication; it
continues without rebasing, and the later source is separately observable.

### 4.3 Kind `0`: independent profile fields survive another device

Device 1 is offline and the app performs:

```text
profile.set_name("Pablo")
profile.set_nip05("_@f7z.io")
profile.save()
```

The profile capability records a semantic operation resembling:

```text
Set name  = "Pablo"
Set nip05 = "_@f7z.io"
```

It does not submit a generic JSON patch for the app to construct. The profile
capability owns that these fields are encoded in kind `0` JSON content.

Suppose device 1 initially knows source E10:

```json
{"name":"Old","picture":"old.jpg","about":"hello"}
```

It can materialize an optimistic successor E11:

```json
{"name":"Pablo","picture":"old.jpg","about":"hello","nip05":"_@f7z.io"}
```

Before delivery completes, device 1 discovers a newer source E12 published by
device 2:

```json
{"name":"Old","picture":"new.jpg","about":"hello"}
```

The accepted operation is replayed over E12. The successor E13 is:

```json
{"name":"Pablo","picture":"new.jpg","about":"hello","nip05":"_@f7z.io"}
```

The picture change survives because it touched a different field. R1 remains
the same receipt; E11 and E13 are distinct materializations. An `OK true` for
E11 cannot be reported as acceptance of E13.

If E12 also changed `name`, Nostr timestamps cannot reveal whether device 2's
name edit causally preceded or followed device 1's offline action. The profile
capability must define the policy: the local set may win, the materialization
may refuse with a conflict, or a future schema may carry stronger causal data.
Generic NMP machinery does not invent that choice.

### 4.4 Kind `3`: several offline operations share one event

This is a replaceable-event example, not a special follow-list architecture.

```text
t+0  device 1 follows Alice while disconnected       → receipt R1
t+1  device 1 follows Bob while disconnected         → receipt R2
t+5  device 2 publishes a kind 3 containing Carol    → source E5
t+8  device 1 reaches its selected sources
```

The follow capability applies “add Alice” and “add Bob” to E5 and returns one
kind `3` body containing Alice, Bob, and Carol. One materialized event E6 may
serve both R1 and R2.

R1 and R2 remain independent receipts because they name two accepted app
actions. They do not require two copies of E6, two signatures, or two sets of
relay attempts.

The timestamp is not blindly the reconnect time `8`. If E5 has
`created_at = 5`, the latest contributing operation time is `1`, and no later
local generation exists, the candidate is:

```text
max(latest operation time 1,
    source created_at 5 + 1,
    previous local generation if any + 1)
= 6
```

If Carol had never appeared and the source was older than both operations, the
candidate could remain `1`. Merely waiting until `t+8` must not mint new bytes.

#### Follow then unfollow before delivery

If the user follows Bob and then unfollows Bob before any generation is
delivered, the effective desired set contains no Bob entry. The capability may
normalize those opposite operations out of the retained program so replay
cost does not grow with canceled history.

Normalization must not erase the receipts. Each app call still needs a truthful
outcome. The exact terminal vocabulary for “the first operation was overridden
before delivery” and “the second operation left the source already in the
desired state” remains an open surface decision. The forbidden answer is to
pretend both receipts independently published two events that never existed.

### 4.5 Kind `30023`: title edit over a concurrent body edit

An app performs:

```text
article.set_title("A better title")
article.save()
```

The long-form capability owns that a title is represented in tags and that the
article body is represented in content. NMP retains only the capability's
versioned operation bytes and target address; it has no hard-coded knowledge
of a `title` tag.

If another device publishes a newer event at the same `(author, 30023, d)`
with an edited body, the capability receives that newer source plus the title
operation. It returns a successor with the newer body and the requested title,
while preserving unrelated tags and unknown extensions according to its
schema policy.

If both devices changed the title, the capability owns the same-item conflict
policy. “Always apply the local operation” is one policy; “refuse because the
base field changed” is another. The generic store and publish scheduler cannot
decide between them.

### 4.6 A private addressable list: required crypto completes before custody

Assume an addressable list stores public items in tags and private items inside
encrypted content. The app requests:

```text
private_list.add(pubkey X)
```

This operation depends on the private entries. Before NMP takes custody, the
configured capability validates the signed source, decrypts the relevant
content, applies the operation, preserves public and private data under its
rules, re-encrypts, and returns a complete unsigned body. NMP derives its event
id before acceptance. If the required capability or crypto cannot complete
that candidate, the call refuses typed before custody and leaves no receipt,
intent, correlation, optimistic row, signer request, route, lane, or durable
waiting state.

That rule does not make opaque encrypted content globally blocking. If the app
instead performs a public-only edit whose meaning does not depend on private
entries, the capability may preserve the exact ciphertext byte-for-byte and
produce a complete candidate without requesting decryption. It must refuse
before custody only when the requested operation actually requires unavailable
private-state knowledge.

The local database is already inside the user's device trust boundary. Durable
plaintext operation data may be stored when restart replay requires it. This
design does not add an encrypted-at-rest operation format or claim that no
plaintext may reach persistence. Transient plaintext still must not leak into
logs, unrelated callbacks, diagnostics, or uncontrolled duplicate buffers.

### 4.7 Kind `10009` simple groups: identity includes the host

NIP-51 defines the kind-10009 “simple groups” list; its entries refer to
host-scoped NIP-29 groups. The capability belongs with group behavior rather
than an omnibus NIP-51 module because group identity is host-sensitive. The
same textual group id at two host relays can describe different groups or
forks.

An operation such as “add this group at host H” therefore cannot be lowered to
“add this string.” The NIP-29 capability owns host-plus-group identity, optional
names, relay-in-use edits, legacy rows, duplicate policy, public/private
partitions, and migration. Generic machinery sees only one target coordinate,
opaque operation bytes, and a returned event body.

This illustrates why a NIP document that catalogs several list encodings is
not automatically one product capability.

### 4.8 A blocked-relay list: optimistic visibility is not security authority

A blocked-relay operation may produce a local signature-pending kind `10006`
row that ordinary queries should show immediately. That does not mean an
unsigned local row is allowed to change process-wide network-admission policy.

The blocked-relay capability can require a validated signed current event
before changing which destinations the engine will connect to. It may also
refuse to describe opaque private entries as an honestly empty block list.

This is an important boundary: effective optimistic query state and a
security-critical side effect can use different evidence requirements without
creating a second read or write lifecycle.

---

## 5. Acceptance and local custody

Acceptance is the point at which NMP tells the app: “this operation is now my
durable responsibility.” It is not the point at which a relay has accepted an
event.

### 5.1 One public acceptance door

Ordinary events, exact replacements, and replayable operations all enter
through the same write-request API and return the same receipt abstraction.
There must not be a second `accept semantic operation` lifecycle beside normal
publication.

### 5.2 What commits atomically

For a replayable operation, acceptance must atomically record at least:

- the stable receipt identity;
- the resolved author identity that may not later be silently retargeted;
- the replaceable or addressable coordinate;
- the exact versioned replay representation;
- the bounded, versioned operation bytes;
- the app's optional correlation value used to recover an ambiguous call;
- the source requirement or selected source-plan identity needed to determine
  what may be materialized;
- the operation's logical time contribution;
- one complete initial current materialization, including its event id;
- the compact relationship between this receipt and any shared current
  materialization.

If this transaction fails, the write was not accepted. NMP must not emit a
receipt that claims custody while keeping the only copy of the operation in
memory.

### 5.3 Body-complete acceptance is mandatory

A replayable operation enters custody only after its configured capability has
produced one complete unsigned event and NMP has derived its event id. Missing
capability code, unresolved source evidence required for the initial candidate,
required crypto unavailability, invalid output, or any other inability to
construct that complete candidate is a typed pre-custody refusal with zero
acceptance residue.

The refusal must not manufacture:

- a fake event id;
- an all-zero or empty signature;
- a half-valid `Event` object;
- a relay destination for bytes that do not exist;
- a query row whose body is only a guess.

An accepted semantic operation therefore begins in the same body-complete
signature-pending condition as an ordinary unsigned event. Later source-driven
work may replace it with a complete successor, but an accepted receipt never
transitions to a bodyless state.

### 5.4 Ambiguous acceptance after a storage failure

If the durable transaction may have committed but the caller receives an I/O
error, blindly repeating the operation could accept it twice. The existing
write contract uses an app-supplied correlation identity to recover the
original receipt when the app retries the call.

Semantic operations keep the same rule. A capability must not invent its own
parallel deduplication lifecycle.

---

## 6. Local visibility before signing

NMP's live query is the app's ordinary read subscription. It observes the
effective local event set, including locally accepted, body-complete writes.
There is no separate optimistic callback or overlay.

### 6.1 Every accepted operation has a complete event row

Accepted operation state has two body-complete forms:

1. **Signature-pending materialization** — complete body and event id exist;
   signature does not.
2. **Signed materialization** — complete validated wire event exists and may
   be delivered.

Both can appear as event rows in a live query. Candidate preparation before
custody is not a receipt state and cannot create a row, event id, or placeholder
signature.

### 6.2 What a signature-pending query row contains

A matching live query receives the complete effective body:

- final author;
- final kind;
- final `created_at`;
- final tags;
- final content;
- final event id; and
- one typed signature property: `Pending`.

It does not receive an empty signature string or a sentinel that could be
mistaken for protocol data. After a valid signature arrives, that same
materialization changes to `Signed(signature)` without changing its body or
event id.

### 6.3 Rematerialization is an ordinary query transition

If a newer qualified source causes E1 to be replaced by E2, matching live
queries receive the same ordinary replaceable-event transition they would for
any winner change: E1 is withdrawn and E2 becomes effective. Derived queries
must update through the same event-store path. There is no direct “tell the
app about the optimistic event” bypass.

The query must retain provenance that distinguishes locally materialized E2
from source event S2 observed at relay A. Optimistic visibility is not relay
evidence.

### 6.4 What queries show while replay over a changed source is waiting

Assume E1 is the body-complete optimistic value and qualified source revision
B2 replaces the source on which E1 was based. The source truth changes
immediately, but capability replay may succeed, wait, or refuse.

| Replay result for B2 | Effective event shown by ordinary queries | Receipt/source evidence |
|---|---|---|
| Complete successor E2 | One logical transition from E1 to E2; B2 never flashes as the effective value | B2 remains source truth; E2 is the local materialization based on it |
| Successor preparation is still in flight or retryable required crypto is temporarily unavailable | Keep E1 as the last body-complete effective value rather than silently erase the accepted operation | State explicitly that E1 is based on a superseded source revision and complete successor preparation for B2 is pending |
| Terminal capability refusal or operation failure | In one logical transition terminalize the affected operation, remove its contribution, and expose B2, qualified absence, or a successor built from remaining operations | Preserve the refusal and the exact source revision that caused it |
| Operation already satisfied by B2 and no successor is required | Expose B2 or the byte-equivalent current result without manufacturing an event | Resolve the accepted operation truthfully without relay-publication claims |

Keeping E1 during a temporary wait does not claim it came from B2 or remains a
relay winner. Its source-staleness must be inspectable through receipt and
source evidence. A future public query-row surface may expose that distinction
more directly, but absence of such a field cannot permit B2 to erase local
meaning silently.

Once B2 becomes the qualified source revision, E1 becomes ineligible for every
new transport handoff and retry even if it remains the last body-complete value
shown in optimistic queries. Work already handed off cannot be unsent; its
result remains historical evidence for E1. If replay later produces E2, only
E2 receives current delivery work.

---

## 7. Choosing a source without claiming a global head

Semantic replay needs a base, but Nostr offers no authoritative global read.
The app or capability therefore selects a bounded source plan: the exact relays,
authors, access contexts, and settlement evidence it considers sufficient.

### 7.1 Source plan and destination plan are different

The **source plan** answers:

```text
Where may the current replaceable value be learned from?
```

The **destination plan** answers:

```text
Where should the resulting signed event be published?
```

They may overlap, but one must not be inferred from the other. Reading a kind
`0` from an indexer does not automatically make the indexer a write relay.
Publishing to an author's write relay does not prove that every selected source
has been reconciled.

### 7.2 “Current” is always scoped

When this document says “selected current source,” it means the NIP-01 winner
among the source evidence admitted by the chosen plan. It never means:

- newest event on all Nostr relays;
- newest event any device has ever produced;
- an event that cannot later be replaced;
- a compare-and-swap lock held on the network.

The system also keeps these states distinct:

- no event is cached yet;
- selected sources have not been reached;
- a selected source is reachable but has not settled;
- every required selected source has settled with no event at the coordinate;
- selected sources disagree and NIP-01 ordering picks one observed winner;
- a cached base is provisionally usable under an availability-first policy.

Creating the first event at a coordinate is capability policy. “Nothing is
cached” cannot silently become proof of an empty source.

### 7.3 Source facts arrive progressively

One source may answer while another is disconnected. The system must retain
which source has yielded which event and which source has reached the plan's
settlement condition. A connection failure and EOSE are different evidence.
NIP-01 has no standalone “absence” message: **qualified absence** means the
declared source plan completed its required initial query evidence—normally
EOSE for each required subscription—without yielding a surviving event at the
coordinate, after applying deletion and expiry rules. It remains scoped to
those sources and that observation interval.

The capability's policy decides when available evidence is sufficient to
materialize. An exact whole-replacement workflow may require every selected
source before composing. Another safe semantic operation may be able to
materialize from the best currently qualified source and remain open for a
possible successor.

### 7.4 Any qualified source revision change while the obligation is active

A greater-NIP-01 winner is the common case, but it is not the only way the
selected source revision changes. A kind `5`, NIP-40 expiry, source withdrawal,
access-policy change, plan membership change, or corrected evidence can expose
a previous event or qualified absence. Every such revision is handled through
the same replay rule while accepted operations remain active:

1. validate any event source as real signed data for the target coordinate and
   validate absence/withdrawal under the exact source policy;
2. select the resulting event, previous winner, or qualified absence under the
   scoped NIP-01 and deletion/expiry rules;
3. replay the compact contributing operations over it;
4. if replay yields a complete successor, install it in one crash-consistent
   logical transition, invalidate results naming the retired materialization,
   sign it, and offer it to every current destination;
5. if replay must wait, keep the last body-complete effective value with
   explicit superseded-source evidence as specified in §6.4; or
6. if replay refuses or is already satisfied, perform the corresponding
   terminal/effective transition from §6.4 without manufacturing a successor.

The predecessor's relay evidence remains historical truth. It simply cannot
settle work for the successor. What deletion, expiry, vanish, or revealed
absence means to the semantic resource remains capability policy; these source
changes cannot all be treated as an empty default document.

An echo of NMP's own current materialization from a relay is source evidence,
but it must not create a rematerialization loop. If the exact event already is
the effective current generation and neither the selected source nor compact
operation meaning changed, no successor is produced.

### 7.5 Planned sources that never become reachable

The system must define a bounded, explicit policy before this orchestration is
claimed complete. Possible policies include an app-controlled source deadline,
a plan that declares some sources optional, or a receipt that remains waiting
until the app cancels it.

No universal answer is chosen here. The required constraints are:

- elapsed time alone must not be described as proof that the unreachable
  source had no newer event;
- offline waiting must not consume relay publication attempts;
- an unresolved source must remain visibly unresolved;
- a product may choose availability over additional source confidence, but
  that policy must be named and observable.

---

## 8. Materialization time and replacement ordering

Every successor must outrank the source it replaces and any prior local
generation at the same coordinate. Reconnecting must not continually change
the event id.

### 8.1 Required timestamp rule

For an automatically stamped successor:

```text
created_at = max(
    latest logical time of every still-contributing accepted operation,
    selected source created_at + 1,
    previous local materialization created_at + 1
)
```

Each term has a separate purpose:

- operation time ensures the successor is not stamped before a contributing
  accepted action under NMP's logical-time policy;
- source `+ 1` makes the successor beat the observed source without relying on
  the event-id tie-break;
- previous local generation `+ 1` prevents a later rematerialization from
  losing to an earlier local event.

### 8.2 Alice, Bob, and Carol

In the earlier kind-3 example:

```text
t+0 add Alice
t+1 add Bob
t+5 source containing Carol
t+8 reconnect
```

The result uses `created_at = 6`, not `1` and not automatically `8`.

- `1` would lose to the source at `5`.
- `8` would make wall-clock reconnection mutate identity even though no user or
  source fact changed.
- `6` is the smallest timestamp that expresses the required ordering.

If no newer source exists, `1` may remain sufficient. If a previous local
generation had timestamp `7`, a successor must use at least `8` even if the
newly selected source is older.

### 8.3 Reconnect never restamps unchanged meaning

Closing and reopening the app, reconnecting a socket, restoring replay
capability, or retrying a relay must not produce a new `created_at` when the
selected source and compact operation program are unchanged. The same meaning
must recover to the same current materialization.

This is necessary for deterministic restart, relay deduplication, bounded
history, and honest receipt evidence.

### 8.4 Future-skew and overflow

A source may carry a `created_at` so far in the future that `source + 1` is
outside accepted policy or overflows the timestamp representation.

Both `selected source created_at + 1` and `previous local materialization
created_at + 1` use checked arithmetic. Overflow of either term is an
unconditional typed refusal for that attempted generation and leaves no partial
body, signature, or delivery work. A configurable future-skew policy may
instead keep the accepted operation waiting until the required timestamp enters
its allowed window, or may terminally refuse it. The receipt must say which
policy acted and whether the operation remains active. NMP must not:

- wrap the integer;
- silently use a lower timestamp;
- fabricate wall-clock convergence;
- publish an event known not to outrank its selected base.

The exact acceptable future-skew window and wait-versus-refuse choice are
product/configuration policy. The result must remain visible and must not be
confused with a signer or relay failure.

### 8.5 The remaining final race

Even after source reconciliation, another device can publish a newer event
between the final read and this client's publication. Nostr provides no
network-wide compare-and-swap. Semantic replay narrows the stale-write window
and preserves newly observed independent changes; it cannot make the final race
impossible.

---

## 9. Capability-owned meaning and loss-preserving transformation

NMP can persist and schedule an operation without understanding its protocol
meaning. The capability that exposes the typed app API owns that meaning.

### 9.1 What the capability owns

For each supported replaceable resource, the capability owns:

- the typed app verbs, such as `set_name`, `follow`, or `set_title`;
- the target kind and address rules;
- field or logical-item identity;
- current encoding in content, tags, or encrypted partitions;
- legacy decoding and whether legacy data is migrated on write;
- preservation of unknown, malformed, duplicate, or extra data;
- normalization of several accepted operations;
- same-item conflict policy;
- ordering and insertion policy within each representable partition;
- whether public-only work may preserve private ciphertext unchanged;
- the difference among add, remove, reorder, clear, reset, delete, expire,
  vanish, and migrate.

### 9.2 What generic NMP machinery owns

Generic machinery owns:

- durable acceptance and receipt identity;
- bounded operation bytes and version identity;
- source selection under the declared plan;
- coordinating capability-defined replay without interpreting its meaning;
- validating the returned coordinate and author;
- timestamp and event-id derivation;
- exact stale-result fencing;
- signing, routing, retry, restart, and evidence;
- compact storage of current materialization and independent receipts.

It does not contain branches for profile fields, follows, article titles,
mutes, bookmarks, pins, or NIP-51 list kinds.

### 9.3 Preservation is part of correctness

An operation that changes one known item must not discard unrelated data merely
because the current implementation does not understand it. Depending on the
capability, preservation may include:

- unknown JSON fields and their exact untouched byte spans;
- tag order unrelated to the edited logical item;
- duplicate tags that are legal or at least already present;
- malformed rows retained rather than “cleaned up” accidentally;
- extra tag cells;
- legacy encodings still readable by existing clients;
- opaque encrypted content during a public-only edit.

A logical item may span more than one consecutive tag. Public tags and private
encrypted items may have independent orders with no meaningful total order
between them. A generic transform must not invent one.

### 9.4 Same-item conflict cannot be solved generically

Consider two devices that both change a profile name. Nostr tells us the order
of the resulting events; it does not tell us the causal order of the user
actions that produced them.

A capability may choose one of several honest policies:

- local operation wins whenever it remains active;
- refuse if the selected source changed the same item since the operation's
  stated base;
- merge a commutative structure such as a set;
- use protocol-specific version or causal metadata if the schema has it.

The generic engine may supply exact source identity and operation order as
inputs. It must not silently choose field-level last-write-wins for every
protocol.

### 9.5 Byte-equivalent application is not a new event

If replaying the compact operation program over a newly selected source yields
the same event fields and required timestamp as the current effective
materialization, NMP must not create, sign, or route another event merely
because replay logic ran.

This differs from an operation detected as a no-op before acceptance. A
capability may return `NoChange` before submitting any write, in which case no
receipt exists. An already accepted operation later discovered to be satisfied
still needs a truthful receipt outcome even if no wire event is necessary.

### 9.6 Clear, delete, expire, vanish, reset, and migrate are different

These verbs are easy to blur and dangerous to conflate:

- **clear a value** usually writes a current event with an empty field or set;
- **remove an item** edits one logical member;
- **reset** may mean replace the current resource with a capability-defined
  default;
- **delete** may require a kind `5` and author/coordinate semantics;
- **expire** uses an expiration policy or tag and does not necessarily delete
  historical evidence;
- **vanish** has different protocol implications from ordinary deletion;
- **migrate** changes representation while preserving capability meaning.

Only the capability can say which of these operations it supports and what
wire events they require. This document's replayable-operation contract covers
one replaceable or addressable target materialization. An action requiring an
additional kind `5`, migration marker, or other side event needs explicit
multi-write composition through ordinary write requests; the current candidate
must not smuggle several publications out of its single-event return. A generic
`clear = delete = reset` shortcut is not acceptable.

---

## 10. Private and encrypted materialization

Encryption changes when a body can exist, not the identity of the write noun.

### 10.1 Decrypt, encrypt, and sign are separate capabilities

A device may be able to sign but not decrypt a source, decrypt but not sign,
or perform a public-only edit without decrypting private content. Before initial
custody, a missing crypto capability required by the operation produces typed
zero-residue refusal; signer unavailability may park the already body-complete
accepted event. Later successor preparation may wait or retry under its
explicit bounded policy while the last complete generation remains current.

The design must never treat “signer available” as proof that content can be
materialized.

### 10.2 Validate before decrypting

Only a validated signed source event may enter the ordinary source-decryption
path. A locally materialized signature-pending row must not be fed back as if a
relay had supplied a verified source.

Every decrypt or encrypt request is bound to:

- the exact source event id;
- the exact target coordinate;
- the exact target materialization revision; and
- the operation program that requested it.

A result for a retired source or materialization is stale even when the receipt
id is unchanged.

### 10.3 Public-only work may preserve ciphertext

If a capability can prove that a public operation does not depend on private
contents, it may preserve the exact opaque ciphertext byte-for-byte. This avoids
blocking a safe public edit on unavailable decryption.

If the operation's meaning depends on private contents—for example removing a
private item—and the required crypto is unavailable during the initial call,
it fails with a typed pre-custody refusal. During later successor preparation,
NMP retains the last complete generation until a complete successor can commit
or the operation follows its typed terminal policy. It must never guess that
the item is absent.

### 10.4 Scheme policy remains capability-specific

The lower mechanism must not globally rewrite every NIP-04 payload as NIP-44.
One list capability may deliberately read NIP-04 and write NIP-44; another
protocol may require NIP-04 or negotiate a scheme with a peer. Detection,
negotiation, self-encryption identity, and migration belong to the capability.

### 10.5 Local persistence and transient secrecy

The NMP database lives on the user's device and is inside the application's
declared trust boundary. Persisting plaintext semantic operation data needed
for crash recovery is allowed.

That does not license uncontrolled copies. Decrypted payload buffers should
have one explicit owner, wipe on release, and never appear in logs,
diagnostics, panic formatting, unrelated capability code, or generic app
callbacks.

### 10.6 Randomized encryption and deterministic recovery

Some correct encryption produces different ciphertext for the same plaintext.
The stable requirement is not that a capability must magically reproduce the
same random bytes before anything commits. It is:

- once a materialized event body is durably installed, restart reuses those
  exact bytes rather than encrypting again;
- entropy or nonce ownership is explicit during materialization;
- a crash before the atomic install may discard an uncommitted candidate;
- stale or repeated callbacks cannot replace an unchanged committed
  materialization with fresh ciphertext;
- a capability format must not silently change how persisted operation bytes
  are interpreted.

This preserves restart identity without imposing deterministic encryption on a
protocol that requires randomness.

---

## 11. One effective event, many receipts, changing event ids

The design permits several operations to share one materialization while
keeping independent receipts. That sentence has two separate claims.

### 11.1 Why sharing a materialization is necessary

Replaceable state is one current event per coordinate. If the user follows
Alice and then Bob before anything is signed, producing and delivering two
whole kind-3 events would be wasteful and can make the first obsolete before
it leaves the device. The effective value after both operations is naturally
one event containing both contacts.

Sharing avoids duplicate full bodies, signatures, route calculations, retry
state, and relay traffic. It also matches Nostr's replaceable-state model: the
network needs the latest complete value, not an event for every local method
call.

### 11.2 Why the receipts remain independent

The app made two independently accepted requests. It may have attached a
different correlation identity, UI action, or business consequence to each.
Compacting the operation program is allowed to remove redundant replay work;
it is not allowed to rewrite history and claim only one request existed.

For example:

```text
R1 = follow Alice
R2 = follow Bob
E3 = kind 3 containing Alice + Bob
```

Both R1 and R2 may say that E3 currently carries their requested meaning. If
E3 is accepted by relay A, that fact can be associated with both receipts
without sending E3 twice. If R2 is cancelled while cancellation is still
permitted, NMP must rematerialize the value required by R1 alone rather than
delete R1's obligation.

Operations may share a materialization only when every event-owning input is
compatible: same author and replaceable coordinate, same qualified source
revision, same capability and replay-format semantics, and one delivery policy
that honestly satisfies every contributing receipt. If two receipts require
different access identities or incompatible source or destination policies,
NMP cannot keep competing effective bodies at one coordinate. It must either
construct an explicitly defined, lossless resource-level union that weakens
neither policy or refuse the incompatible second request before `Accepted`.
Matching only on coordinate is insufficient. A destination union is not
automatically lossless when one operation's policy intentionally limits where
its resulting state may be disclosed.

### 11.3 The acceptance event id is initial, not permanently current

Every accepted operation has an initial event id. Later, one operation may move
through E1 and E2, while two receipts may share E3. A mandatory initial
`receipt.event_id` is therefore truthful, but interpreting that field as the
receipt's one permanent current event id would encode a false one-to-one
relationship.

The target receipt must retain its initial accepted event id and expose later
event identity as generation-qualified facts equivalent to:

```text
materialized E1
signed E1
relay A accepted E1
retired E1 because source S2 arrived
materialized E2
signed E2
relay A accepted E2
```

The final public spelling is a later surface choice. The required invariant is
that every event-specific fact names its exact event id and materialization
revision.

### 11.4 One global successor, not a different body per relay

NMP must not quietly create independent replaceable histories for each
destination. At any point, one coordinate has one current effective
materialization. If a newer source causes E2, every current destination is
offered that same E2.

A relay that accepted E1 must receive E2 again. A relay that never saw E1 may
receive only E2. Both paths converge on the same current event bytes.

---

## 12. Signing and stale-result exclusion

Signing may be local, remote, hardware-backed, or temporarily unavailable.
The write remains owned by NMP while it waits for the exact identity selected
at acceptance.

### 12.1 Identity freezes at acceptance

Changing the app's active account later must not re-author an accepted
operation. The receipt remains bound to the exact author coordinate selected
when NMP took custody.

### 12.2 A signature is valid only for one materialization

NIP-01 event ids exclude the signature, so promoting one materialization from
signature-pending to signed does not change its event id. But rematerialization
does change body bytes and event id.

Every signer request and result must therefore carry the exact materialization
identity. If E1 is retired while a remote signer is still working, a perfectly
valid signature for E1 must not populate E2. The stale result is ignored or
reported as stale; it cannot mutate current state.

The same rule applies to decryption, encryption, route calculation, transport
handoff, and relay responses.

### 12.3 Missing signer follows body-complete acceptance

If E1 already exists but its signer does not, the receipt says it is waiting
for the signer tied to E1's author. Matching live queries may see E1 as
signature-pending.

If no complete initial body can be produced, NMP refuses before custody and
there is no receipt to park. Mixing pre-custody candidate preparation with
signer waiting would let an app believe NMP had accepted a valid event when it
had not.

### 12.4 Signer and crypto outcomes

| Outcome | Receipt behavior | Effective query behavior |
|---|---|---|
| Signer temporarily absent after acceptance | Remain open and name the signer tied to the complete current event | Keep the body-complete signature-pending value |
| Required crypto unavailable during initial candidate preparation | Typed pre-custody refusal; no receipt or other acceptance residue | No optimistic row |
| Required crypto temporarily unavailable while preparing a source-driven successor | Keep the receipt open under the explicit bounded successor policy | Keep the last complete generation, marked against its retired source revision; never install a bodyless replacement |
| Signer explicitly refuses this current materialization | Terminalize every operation whose only current publication depends on that refusal, unless capability policy supplies another valid path | Remove the refused materialization's local contribution in one logical transition and reveal qualified source state or a value rebuilt from remaining operations |
| Returned signature or crypto result is invalid | Treat as typed capability failure, never as relay failure or success | Do not promote or mutate the current row; terminal compensation follows the same rule as explicit refusal if no retryable provider path remains |
| Result is valid but names a retired materialization | Record or discard as stale according to diagnostics policy; do not terminalize the current operation | No query change |
| Provider execution fails transiently | Remain waiting or retry under the bounded capability policy | Keep the last honest effective state; expose the wait through the receipt |

When several receipts share one materialization, a signer refusal concerns that
one event, not proof that every semantic operation is inherently invalid. NMP
must either rebuild the remaining operations under another permitted signing
path or terminalize the affected co-owners explicitly. It cannot compensate one
receipt by silently deleting other receipts' contributions.

---

## 13. Progressive routing and delivery

Semantic rematerialization produces ordinary signed Nostr events. Those events
use the same routing, connection, authentication, retry, and receipt-evidence
machinery as any other publication.

### 13.1 Known destinations proceed independently

Routing knowledge can arrive gradually. If the configured app relay, the
author's write relays, and four recipients' inbox relays are known while a
fifth recipient's relay list is still unresolved, ready destinations need not
wait for that unrelated lookup.

Once a signed current materialization exists, NMP may offer it immediately to
destinations whose route and access prerequisites are ready. The unresolved
recipient remains visible as unresolved work. This is not permission to close
the destination plan early or forget the fifth recipient.

The same principle applies when one known relay is not connected. The engine's
connection owner establishes a bounded relay session when capacity permits. A
temporarily unavailable connection or bounded transport capacity means
waiting, not that the event or destination became invalid.

### 13.2 Destination selection remains bounded and policy-governed

A relay list supplied by another user is untrusted input. A malicious kind
`10002` containing a thousand URLs must not cause a thousand connections or
publications. Recipient fan-out needs a finite per-recipient target and a
finite global connection envelope. Selection may prefer an already connected
or already-planned relay and try an alternate when the preferred candidate
cannot connect.

The exact recipient-selection algorithm belongs to the routing design, not to
semantic operation materialization. This document requires only that semantic
operations use it rather than bypass it.

Likewise, a relay excluded by an explicit app-owned user rule must remain
excluded no matter whether it came from an app route, an author's relay list,
a `p`-tagged recipient, an event hint, or a rematerialized event. NMP itself
does not classify a destination by its resolved address or hostname; connection
success or failure remains an observed transport outcome.

### 13.3 Parent-event relay evidence is a routing concern

If the ordinary automatic routing policy says a reply should include a relay
where NMP actually observed its referenced parent, semantic operations must
preserve that behavior. A relay URL merely authored into the parent `e` tag is
not verified observation provenance and does not by itself widen the route.
Capability replay does not decide this policy and must not silently drop the
parent reference while rebuilding event content.

Conversely, replayable replacement does not make every source relay a write
destination. Source evidence and destination policy remain separate inputs.

### 13.4 Each relay destination is one event-qualified obligation

For each exact `(event id, relay, access context)`, the engine records a
progressive state such as:

- waiting for connection capacity;
- waiting for relay authentication;
- eligible to send;
- socket write and flush completed;
- waiting for the relay's correlated `OK`;
- retry scheduled after a transient failure;
- relay rejected the event;
- relay accepted the event;
- bounded attempt ceiling reached.

The word **lane** is sometimes used internally for this one-event-to-one-relay
obligation. This document avoids relying on that shorthand.

### 13.5 What `OK true` proves

A correlated NIP-01 `OK` with `true` proves that this relay accepted the event
identified by that exact event id under its policy at that time. Because an
event id commits the author, timestamp, kind, tags, and content but not the
signature field, this is evidence about the submitted event identity, not a
byte-for-byte echo of the wire frame.

It does not prove:

- the event is current on every relay;
- no newer event will arrive later;
- the source plan is globally complete;
- another device cannot supersede it;
- a readback would necessarily return it forever.

The target does not add a mandatory `ObservedCurrent` readback phase. Delivery
evidence stops at what was actually observed. An app may separately issue a
live query if it wants stronger source-scoped evidence later.

No `OK false` becomes success merely because a human message sounds positive.
Section 21 records one existing NMP interoperability rule for the standardized
`duplicate:` prefix.

### 13.6 Retry is bounded per destination

Transient send, timeout, and relay failures use the ordinary durable retry
policy. A finite attempt ceiling terminates that relay's work without changing
other relays' states. Time spent offline or awaiting authentication consumes no
publication attempt because no event was offered.

The signed bytes for one materialization remain fixed across its retries. A
reconnect does not restamp or re-sign merely because time passed.

### 13.7 Successor re-fanout

If E1 was offered or accepted at some destinations and a newer source causes
E2 while the semantic obligation is still active:

- E1's future unsent/retry work is retired immediately; transport already
  handed off cannot be unsent;
- any ambiguous historical handoff evidence for E1 remains attributed to E1;
- E2 receives a fresh destination obligation for every current planned
  destination;
- E1's acceptance at relay A does not settle E2 at relay A;
- E2's result at relay A does not rewrite E1's historical fact.

This exact event qualification is the mechanism that prevents a stale `OK`
from completing a successor.

### 13.8 Destination changes between generations

The planned destination set may gain or lose relays as route knowledge changes.
Historical facts remain attached to the event and relay that produced them.
The settlement denominator for the current generation is the current closed
destination plan, not every URL ever considered.

A newly added destination receives the current generation, not every retired
predecessor. Removing a not-yet-attempted destination retires its current work
under the routing policy; it does not erase an earlier socket handoff or relay
response. Each successor receives a fresh bounded attempt budget. Retired
predecessor timers and retry ownership must be unable to send again.

Route-only discovery does not rematerialize. If the source revision and compact
operation meaning are unchanged, learning a new destination preserves the
current event id and signature, adds only that destination's obligation, and
does not resend to an already-terminal relay.

Authentication evidence is also session- and identity-specific. An `OK` or
AUTH result from an older connection generation or different signing identity
cannot advance the current event-to-relay obligation merely because the URL is
the same.

---

## 14. When a receipt settles

A receipt is open while NMP still owns meaningful source, materialization,
signing, routing, or delivery work for the operation.

A materialization finishing all of its relay work is not automatically the
same as the semantic operation becoming terminal. The operation can remain
active under a declared source policy that still permits a newer qualified
base and successor. Conversely, a bounded one-shot policy may close both once
its source and destination plans are terminal. The policy must say which case
applies.

### 14.1 Relay acceptance is progressive evidence, not the whole result

One relay accepting E1 can be useful immediately in the app's UI. It does not
necessarily end the receipt while another current destination is retrying or a
declared source remains unresolved under the chosen policy.

The intended whole-write boundary is:

```text
the selected source plan is closed,
the destination plan is closed,
and every destination for the current materialization is terminal
```

Terminal destination states include acceptance, permanent rejection, and a
bounded give-up result. The aggregate result reports the exact mixture; it does
not reduce it to a misleading global success boolean.

A closed destination plan with no admissible destination never satisfies
publication successfully by vacuous truth. If the semantic source plan is also
closed, it terminates as `NoDestination`—a not-sent outcome carrying the
routing refusals or absence that caused it. If a deliberately continuing
source plan remains open, the receipt instead stays open with an explicit
“current generation has no destination” fact: a later source revision may
change preserved tags and therefore the route of a successor.

Delivery terminality does not retract a valid signed local event:

| Delivery result | Receipt | Effective query value | Retained semantic work and later sources |
|---|---|---|---|
| Some destinations accept; others reject or give up | Exact per-relay facts; current generation's delivery is terminal when every destination is terminal | Keep the signed local materialization visible | Under a closed one-shot source policy, compact the replay program after terminal operation outcomes and never reopen it; under an explicitly continuing source policy, keep the operation active for a possible successor |
| Every nonempty destination rejects or gives up | Same truthful aggregate; never call it global success | Keep the signed local materialization because relay refusal does not invalidate its signature or body | Same one-shot-versus-continuing source-policy distinction; later source revisions act only while the operation deliberately remains active |
| Destination and semantic source plans both close while the destination set is empty | Terminal `NoDestination`, with no sent or accepted claim | Keep the last valid local materialization visible | Close this publication request; retain bounded receipt evidence, compact terminal semantic work, and never resurrect it merely because routing/configuration or source evidence changes later |
| Current destination plan is empty but a deliberately continuing semantic source plan is still open | Remain open; report that the current generation has no destination, never success | Keep the last valid local materialization visible | A later qualified source revision may create a successor whose preserved fields imply different destinations; route/configuration changes alone follow the explicit route-revision policy |
| Materialization never becomes validly signed | Signer/crypto outcome from §12.4, not a delivery result | Compensate to qualified source state or a value rebuilt from remaining operations | Never create relay work for invalid or absent signed bytes |

If the product later wants to retry after `NoDestination` or after a closed
one-shot failure, it submits a new write request after changing the relevant
route or source policy. The old receipt remains an immutable explanation of
what happened.

### 14.2 No mandatory readback terminal

The receipt does not remain open forever waiting for a later query to return
the event as current. That would mix publication with a new read policy and
could never prove permanence.

### 14.3 A settled receipt is not silently resurrected

Once the declared source and destination policy has closed and the receipt is
terminal, an unrelated source discovered by some future query must not silently
reopen old publication work. If a product wants continuous reconciliation, it
must choose and expose a policy that keeps the semantic obligation active.

The exact boundary between a bounded one-shot source plan and a deliberately
long-lived reconciliation policy remains an open orchestration decision. It
must be explicit before the end-to-end feature is called built.

### 14.4 Exact-base refusal

For a stale exact whole-event replacement, NMP records one terminal refusal
receipt with expected and actual base ids. It creates no optimistic event,
signer request, destination work, or retry obligation. This reflects current
custody behavior and corrects older documents that claimed no receipt existed.

---

## 15. Cancellation, supersession, and operation resolution

These are related but not interchangeable.

### 15.1 Cancellation is an explicit app request

The app asks NMP to stop an accepted operation. Today, ordinary NMP writes are
cancellable only before signing. A signed write returns a typed refusal because
the event may already have escaped the device.

Replayable operations make the boundary more subtle. One materialization may
serve several receipts, and later complete successors may retire an earlier
event id. The target must define cancellation in terms of operation
contribution, not merely “delete this event id.”

When cancellation is still safe:

1. remove that operation's contribution;
2. normalize the remaining operation program;
3. rematerialize the effective value if necessary;
4. compensate the optimistic query view atomically; and
5. retain a truthful terminal cancellation fact for that receipt.

If a shared current generation is already signed or may have crossed a
transport handoff, the allowed result requires explicit design and proof. NMP
must not promise to unsend bytes or silently cancel co-owner receipts.

### 15.2 Supersession is a newer effective materialization

E2 supersedes E1 when the same active operation set is rematerialized over a
newer source, or when the retained operation program changes. E1's body and
future retry ownership can be retired, but historical evidence about E1 remains
truthful and event-qualified for as long as receipt-retention policy requires.

### 15.3 Operation resolution and compaction

An operation no longer contributes when capability-defined normalization or a
terminal outcome proves its semantic work is complete, overridden, cancelled,
or already satisfied.

Heavy operation bytes may then be compacted away. The receipt's small outcome
and historical event/delivery facts remain independently retained under the
ordinary receipt policy.

Compaction must not infer semantics from opaque bytes. The capability supplies
the normalization rules when it creates the versioned replay representation;
the selected replay mechanism yields the normalized still-contributing program
and explicit per-receipt resolution facts.

While an individual receipt remains cancellable or otherwise capable of
changing the effective value, compact state must retain enough
capability-defined contribution information to remove that receipt and
recompute the others. Follow Bob plus unfollow Bob cannot be collapsed to
irreversible “nothing” while both calls still advertise independent
cancellation. Heavy bytes may disappear only after their semantic contribution
is represented reversibly in the compact program or the receipt has reached a
terminal state that no longer permits cancellation.

### 15.4 Open surface decision: normalized-away operations

Follow Bob followed by unfollow Bob, or setting a field to the value already
present in a newer source, can produce no wire change. The design requires a
truthful terminal result for each accepted operation but does not yet choose
the public names.

Possible distinctions include overridden, satisfied-without-publication, and
cancelled. They must be judged against real app questions before adding types.
YAGNI applies: no state is added merely because it can be imagined.

---

## 16. Restart and replay configuration

Durability matters most when the process disappears between phases.

### 16.1 What survives restart

For every active coordinate, recovery must reconstruct:

- independent open receipt identities and correlation values;
- the compact still-contributing operation program;
- the exact versioned replay format and configured implementation identity;
- selected source revision and source evidence needed by the plan;
- exactly one complete current materialization;
- monotonically non-reused materialization identity;
- exact signature, route, attempt, and relay facts for the current event;
- bounded historical facts required for retired event ids.

Recovery of unchanged state must not rewrite it merely to rebuild in-memory
ownership.

### 16.2 Replay capability is required engine configuration

When separately packaged capability code owns replay, the exact implementation
and format needed by durable operations must be configured before the engine
opens and resumes them. Missing or mismatched code is a typed startup or
configuration failure; it is not a receipt state, does not create a bodyless
accepted operation, and is not repaired by late registration wakeup. The store
remains intact so the correctly configured engine can open it later.

A different version must not guess that it can decode the bytes. Exact format
mismatch remains distinct from an implementation that is not configured, but
neither becomes durable waiting work inside the ordinary receipt lifecycle.

### 16.3 Replay outcomes remain exact

After configuration succeeds, the receipt can distinguish a permanent
capability refusal, retryable successor preparation failure, internal execution
failure, and a stale result discarded because the source or operation changed.
None of those outcomes may erase the last complete generation and replace it
with a bodyless state.

### 16.4 Materialization identities never reuse

A coordinate may become inactive after every operation resolves and later
receive a new operation. Its internal materialization sequence must not restart
at an old value. Otherwise a delayed signer or crypto result from the previous
lifetime could accidentally match the new state.

The durable state therefore needs a non-reuse mechanism that survives resource
inactivity and restart. The physical representation is an architecture choice.

---

## 17. Atomicity across source and effective state

When a newer source event B2 arrives, two truths must become durable together:

- B2 is now the selected source-qualified event; and
- applying the still-contributing local operations to B2 yields effective
  complete materialization E2, or follows the explicit successor
  retry/refusal policy while E1 remains the last complete effective value.

Ordinary app queries must never briefly expose raw B2 as the effective value
and then switch to E2 in a second transaction. That flash would momentarily
erase the user's accepted local operation from the app's own state.

At the same time, diagnostics or source-qualified observation must still be
able to say that B2 came from relay A and that E2 is a local materialization.
“Atomic effective update” must not destroy provenance.

Source evidence may advance while successor preparation runs outside store
locks, but that immediately makes E1 ineligible for new handoff and retry. The
crash-consistent logical transition that installs complete E2 must retire E1 as
current, move receipt membership, and fence stale completions. A crash leaves
either complete E1 with the newer source evidence and explicit successor state,
or the complete successor E2, never a bodyless or mixed generation. This
requirement does not dictate whether a backend implements that authority
boundary as one physical database transaction or another equally strong atomic
protocol.

---

## 18. Bounded work and storage

Correct semantic replay cannot make one hot replaceable resource grow without
bound as the user edits it.

### 18.1 Cost follows unresolved meaning, not historical actions

Capability-defined normalization may reduce:

```text
set title A; set title B; set title C
```

to the still-contributing operation “set title C,” while keeping receipt
outcomes for all three accepted actions. Likewise, add X then remove X may
leave no current set delta.

Rematerialization cost should be proportional to the compact unresolved
program and the current document, not every operation ever accepted.

### 18.2 One current body per active coordinate

The store keeps one full current materialization for an active coordinate,
plus only bounded historical evidence required by receipt and ambiguity
contracts. It does not retain every predecessor body, route, retry deadline,
and signature forever.

### 18.3 One resource change does not inspect unrelated resources

A newer profile source must rematerialize that profile without inspecting every
pending contact list, article, relay list, and bookmark set. Recovery may visit
currently open resources, but unchanged recovery performs no durability
commit.

### 18.4 Untrusted inputs stay bounded

Bounds apply to:

- operation byte size;
- decoded document size;
- plaintext and ciphertext size;
- recipient relay candidates and selected destinations;
- relay connections;
- retry attempts;
- retained historical receipt facts.

An implementation using app-registered replay code must additionally bound
registrations, executor concurrency, and waiting work.

A capability or remote event exceeding a bound receives a typed refusal. NMP
must not silently truncate semantic input and pretend the intended operation
was applied.

### 18.5 Source churn does not create unbounded concurrent generations

Every signed successor needs fresh event-qualified delivery work, but rapid or
malicious source changes must not create an unbounded pile of materializations.
At one coordinate NMP keeps one current materialization and bounded in-flight
candidate work. If several source revisions arrive before a candidate is
signed or handed off, it coalesces directly to the newest qualified revision
and fences intermediate candidates rather than publishing each one.

Once a generation may have crossed a transport handoff, its historical fact
cannot be erased as though it never existed. Full predecessor bodies, timers,
and retry ownership may still be retired; only bounded evidence remains.

Over an infinitely long reconciliation policy with infinitely many genuine
source changes, total lifetime network work can also be infinite. The system
can bound concurrent work, retained state, rate, and the declared lifetime of
an obligation; it cannot promise a finite historical total while deliberately
remaining active forever. The final source policy must therefore choose a
lifetime or budget and expose when it ends.

When old delivery history is pruned under retention policy, inspection must
carry an explicit “earlier evidence pruned” boundary or retained summary.
Missing detailed facts must never be interpreted as proof that a predecessor
was never attempted.

---

## 19. Protocol limits and deliberate trade-offs

This design improves local correctness without claiming powers Nostr lacks.

### 19.1 No global head

Every “current source” claim is scoped to a declared source plan. Other relays
or devices may know a newer event.

### 19.2 No network compare-and-swap

A relay does not reserve the replaceable coordinate between this client's read
and write. The final fetch/publish race remains.

### 19.3 No generic causal order

Two signed events provide timestamp ordering, not necessarily causal ordering
of the underlying user actions. Same-item conflict remains capability policy
unless the schema carries stronger metadata.

### 19.4 No winner permanence

`OK true` is evidence that one relay accepted one event. It cannot prove that
the event will remain current there or elsewhere.

### 19.5 Availability versus source confidence is policy

Waiting for every planned source reduces some stale-base risk and may prevent
all progress when one relay is unreachable. Publishing from currently qualified
sources improves availability and may require successor re-fanout later. The
system must expose which policy was chosen and what remains unresolved.

### 19.6 Replay can preserve only meaning the capability models

Unknown bytes can often be preserved exactly, but the capability cannot merge
two concurrent semantic changes it cannot identify. Exact replacement remains
the honest fallback.

---

## 20. Target behavioral checklist

An implementation satisfies this document only if all of these statements are
true through the ordinary public write and query APIs:

1. A regular kind `1` follows the fixed-event path and never invokes replaceable
   source or semantic replay work.
2. An exact whole replacement either accepts against its exact base or leaves
   only a terminal typed refusal receipt.
3. A replayable operation is accepted only with one complete initial unsigned
   event and event id; inability to construct it refuses before custody with
   zero acceptance residue.
4. A body-complete optimistic event appears through ordinary matching live
   queries before signing, with one typed pending-or-signed signature property.
5. No empty or sentinel signature crosses the public event-row boundary.
6. One receipt may name successive event ids; several receipts may share one
   current event id.
7. Every crypto, signature, route, attempt, and relay fact is fenced by the
   exact materialization it concerns.
8. A newer source is never transiently exposed as the effective app value
   before local operations are reapplied.
9. A successor's timestamp obeys the three-term monotonic rule and reconnect
   alone never restamps it.
10. Overflow of either required timestamp increment—source `+ 1` or previous
    local materialization `+ 1`—produces a typed refusal; a future-skewed source
    follows the explicit typed wait-or-refuse policy rather than producing a
    losing or wrapped event.
11. Ready destinations can proceed without waiting for unrelated unresolved
    routing knowledge.
12. A successor is re-offered to every current destination, including relays
    that accepted its predecessor.
13. Relay acceptance remains qualified by exact relay and exact event id.
14. The source plan and destination plan are independently visible and never
    described as global completeness.
15. Replay representation survives restart, and the exact implementation and
    format are required engine configuration rather than a missing-code receipt
    state or late-registration wakeup path.
16. Capabilities own logical identity, preservation, normalization, conflict,
    deletion/reset/migration, and encryption policy.
17. Private operations never guess when required plaintext is unavailable; the
    trusted local store may persist operation plaintext needed for replay.
18. Opposing or redundant operations may compact without losing independent
    receipt truth.
19. One coordinate's change neither scans nor rematerializes unrelated
    coordinates.
20. Restart does not reuse materialization identities, rewrite unchanged state,
    or resurrect retired event delivery.
21. Safe cancellation removes only the requested receipt's contribution and
    cannot silently cancel co-owners of a shared materialization; post-signature
    authority has an explicit typed policy.
22. Source-plan lifetime, receipt terminality, operation-time clock, first-value
    policy, and pruned-history evidence are explicit rather than hidden behind
    “online,” “synced,” or elapsed time.
23. Route-only changes preserve event id/signature and add or retire only the
    affected destination work; they do not masquerade as source rematerialization.
24. The final payload either structurally excludes unsigned blind replacement
    or names it honestly as an unsafe low-level publication rather than a
    capability-owned edit.
25. A source revision change immediately stops predecessor handoff/retry even
    when that predecessor remains the last honest optimistic query value while
    replay waits.
26. An empty destination plan never counts as successful publication; it
    becomes terminal `NoDestination` with routing evidence only when the
    semantic source plan is also closed.

This checklist is not sufficient while §23 remains open. Final implementation
must resolve each applicable open decision with a named policy and falsifier,
or explicitly remove the unsupported behavior from scope.

---

## 21. Current NMP behavior versus this target

This section is a truth anchor for the transition. It should be deleted or
rewritten when the target becomes built; it is not a compatibility promise.

| Question | Current NMP | Target in this document |
|---|---|---|
| What can one accepted write contain? | One complete body and one permanent event id | A complete fixed body, an exact replacement, or a durable operation with one complete initial materialization and possible complete successors |
| What does a receipt identify? | In practice one fixed event publication | One accepted app operation across one or several complete materializations |
| Can several receipts share one event? | Only limited identical-byte co-ownership; no semantic program | Yes, while keeping independent operation outcomes |
| Can a newer source automatically rebase an accepted write? | No | Yes, while the declared obligation remains active |
| What does a live query see before signing? | Complete pending body with separate signature string and state fields | Complete body only, with one public `Pending | Signed(signature)` property |
| What happens on an exact replacement conflict? | Terminal refusal receipt; no pending row, signing, route, or delivery work | Same exact fallback behavior |
| When is `created_at` chosen? | Ordinary/exact builder acceptance may use current clock and current local winner | Replay successor uses operation/source/prior-generation maximum; reconnect alone does not restamp |
| What if replay code is unavailable? | No semantic-operation payload exists | Initial use refuses before custody; restart/open reports typed configuration failure and retains the store intact, with no missing-code receipt state or late wakeup |
| Can relay evidence outlive a predecessor? | One receipt assumes one event id | Yes, but every fact remains scoped to its predecessor event id |
| Does relay `OK true` prove convergence? | No | No; no mandatory readback state is added |

Two older descriptions are specifically stale and must not be carried into the
new implementation:

- older write documentation says a replaceable conflict allocates no receipt;
  current NMP deliberately retains a terminal refusal receipt;
- older write documentation mentions an at-most-once durability mode and
  `OutcomeUnknown`; current durable restart retries the exact same frozen event
  bytes instead.

One current interoperability rule is intentionally stronger than a literal
reading of NIP-01's examples: an exact correlated `OK false` with the
machine-readable `duplicate:` prefix is classified as publication evidence for
that event id, because the relay reports that it already has the event. NIP-01
shows a duplicate example with `OK true`; NMP accepts the widely encountered
false-plus-duplicate form narrowly. Free-form text containing “duplicate” does
not qualify, and every result remains relay-, session-, identity-, and
event-id-specific.

---

## 22. Production direction and historical experiment evidence

Everything before this section is the behavior an implementation must serve.
The dependency direction in this section was settled on 2026-08-13: NMP owns a
small materializer contract and independently packaged capabilities supply the
semantics. The exact production surface remains volatile and does not make the
target built. The bodyless #1412 prototype described later is historical
evidence only and is explicitly rejected as production architecture.

### 22.1 Keep one public write noun

NMP's app-facing write noun remains `WriteIntent`: one value that asks the
engine to take custody of a publication obligation. Today it contains:

```text
payload
routing policy
identity selection
optional app correlation
```

Today the payload is one of:

```text
Event(builder)
ReplaceableEdit { builder, expected_base }
Signed(event)
```

The candidate hard cut reorganizes replaceable payloads without adding a third
app-facing lifecycle:

```text
WritePayload
├── Event(builder)
├── Replaceable
│   ├── Exact { builder, expected_base }
│   └── Operation {
│         target coordinate,
│         source requirement,
│         materializer key,
│         operation format,
│         bounded opaque operation bytes
│       }
└── Signed(event)
```

This is illustrative. The essential shape is that a complete builder and a
semantic operation are mutually exclusive authorities.

`Exact` above is mechanism input minted only by a capability workflow after it
establishes source and preservation policy. It is not a raw app-constructible
payload. The final surface must also resolve §3.1's open question about deleting
unsigned blind replaceable builders.

The replay representation does not itself mint author, `created_at`, event id,
or signature. Author comes from the write's identity selection. The configured
materializer and NMP derive the complete candidate timestamp and event id before
custody; signature follows after acceptance.

### 22.2 Configured capability materializers

The application assembles independently packaged capability modules and
configures exact materializer implementations with NMP before use. NMP depends
only on a small contract; it does not depend on every profile, follow, article,
list, or third-party capability crate.

Conceptually:

```text
application startup
    ├── register profile materializer, format 1
    ├── register follow materializer, format 1
    └── register long-form materializer, format 2

capability operation factory
    └── is bound to exactly one configured materializer key + format
```

The registration key identifies semantic ownership. The format identifies the
exact durable byte contract. A materializer registered under another format
must not receive unknown bytes.

Configuration is a prerequisite, not a receipt lifecycle. A supported app must
not mint or transplant raw replay authority. Missing implementation or format
causes typed refusal before custody for an initial call, and typed engine-open
failure for already durable work. There is no late-registration wakeup queue.

This dependency inversion avoids a static `nmp -> every capability crate`
graph, which is rejected: it would make core NMP the catalog owner for every
replaceable-event capability and require changing NMP to add third-party
modules.

### 22.3 Materializer contract

The candidate materializer is a deterministic transformation:

```text
input:
  qualified source revision:
    validated selected signed event, or
    typed qualified absence/withdrawal/deletion/expiry reason
    plus source-plan and access-context identity
  compact normalized opaque operation bytes
  target coordinate and target created_at

output:
  complete unsigned event body for exactly that target
  explicit per-operation resolution/normalization facts

or:
  already satisfied, with per-operation resolution facts and no new event
  typed refusal
```

For initial custody, `already satisfied` must still resolve the call without
inventing an accepted bodyless obligation; if the operation requires
publication, only a complete candidate can be accepted. Unresolved source or
required crypto is handled before this acceptance decision and produces typed
zero-residue refusal when it prevents a complete initial candidate.

“Deterministic” here means semantically stable for the same source, operation
program, format, and target time. A capability whose correct wire encoding uses
random entropy may produce different uncommitted candidate bytes. Once one
candidate body commits as the current materialization, NMP persists and reuses
those exact body bytes; restart never invokes encryption merely to recreate
them.

The materializer owns schema semantics but not publication. It cannot select a
different author, route, signer, or receipt. NMP validates every returned
field that belongs to the generic contract.

Unresolved source work never enters the materializer disguised as `None`.
Orchestration first distinguishes unresolved evidence from a qualified source
revision. The capability may then treat first-value creation, revealed
predecessor, deletion, expiry, withdrawal, and settled absence differently
without learning how NMP schedules queries.

For an exact base, exact operation bytes, exact format, and exact target time,
the capability must provide stable semantics. Reusing a format identifier for
a different interpretation is forbidden. If physical encoding legitimately
uses randomness, NMP persists the first committed complete body and never
reruns the encoder merely because the process restarted.

### 22.4 Execute capability code outside store locks

Application-selected capability code may be slow, fail, or panic. It must not
run while the durable database transaction or engine's serialized state owner
is held.

Initial candidate preparation runs before the acceptance transaction. Later
source-driven successor preparation uses the same off-lock discipline. The
successor flow is:

```text
1. Point-read the target's exact source revision, current generation,
   compact operation program, and program digest.
2. Schedule the materializer on a bounded executor.
3. Run capability code outside persistence locks.
4. Return the candidate result to the engine.
5. In one short transaction, compare the exact source revision, current
   generation, program digest, and requested successor identity.
6. Commit only if every fence still matches; otherwise discard the stale
   result and reschedule from current state.
```

A receipt id alone is never a sufficient fence because the same receipt may
survive several generations.

Configured materializers are trusted application code, not a sandbox boundary.
Catching a Rust panic cannot contain process abort, unsafe memory corruption,
unbounded allocation, or an infinite loop. Executor capacity, cooperative
cancellation, deadlines, shutdown, and native callback lifetime therefore need
explicit policy and falsifiers before this mechanism can be production
architecture.

For the initial call, panic, invalid output, refusal, or required-capability
unavailability leaves zero acceptance residue. Bounded execution machinery may
be needed for successors, but it must not add missing-handler or bodyless
receipt states.

### 22.5 Durable state shape

The leading store design uses one record per active replaceable/addressable
coordinate containing:

- current source revision;
- compact opaque operation program;
- independent receipt membership and outcomes;
- exactly one complete current materialization while operations are active;
- monotonic materialization high-water identity;
- current-generation signature fence;
- indexes that let one source change point-read only its coordinate.

Active accepted state has exactly one current unsigned-or-signed
materialization, not separate fields that can disagree. Successor preparation
may be in flight while the last complete generation remains current. Resolved
heavy operation bodies can be deleted while small receipt evidence survives.

### 22.6 Receipt and delivery projection

The current direct-Rust `ReceiptStream` has a mandatory `event_id`, which is
truthful for body-complete acceptance, while the current publish queue assumes
that id remains permanently current. Production must hard-cut only the latter
assumption:

- acceptance returns a receipt with its exact initial event id;
- successor materialization facts introduce later exact event ids;
- signature facts name the exact event id/materialization;
- relay facts name exact event id, relay, and attempt;
- the aggregate result reduces the current-generation terminal facts without
  rewriting predecessor history.

The old shape should be replaced, not kept as a nullable compatibility alias
beside a new state owner.

### 22.7 Comparison: registered semantic operations versus closed EventEdit

The alternative under evaluation compiles capability operations before
acceptance into a closed, capability-neutral structural edit format.

| Property | Registered semantic materializer | Closed structural EventEdit |
|---|---|---|
| Who owns conflict meaning at replay time? | Capability implementation | Whatever can be expressed in the closed edit language |
| NMP dependency on capability crates | None; app assembles registrations | None after capability compiles the edit |
| Restart requirement | Exact implementation and format are required engine configuration | Generic interpreter is always present |
| Missing-code state | Typed configuration/open failure; never an accepted receipt state | Not required for supported plan versions |
| Expressiveness | Arbitrary deterministic capability policy | Bounded by structural instruction set |
| Operational risk | Callback scheduling, panic, slowness, native packaging | More generic machinery and risk of semantic leakage into the IR |
| Versioning | Capability-specific opaque format | Central structural plan version |
| Native SDK integration | Unproven for arbitrary callbacks | Easier if native apps call fused typed helpers only |

The dependency-inverted materializer direction is selected because it keeps
capability semantics out of generic NMP without adding a static dependency on
every capability crate. The closed structural `EventEdit` remains useful
comparison evidence, not the selected universal semantic language.

### 22.8 What the rejected #1412 prototype proved

The #1412 experiment has two evidence layers. Its bodyless lifecycle and
parallel persistence were rejected, but its first isolated registry prototype
still proved that:

- two independently packaged capability implementations can register through
  one NMP-owned contract;
- the NMP mechanism has no dependency on either capability;
- exact materializer and format identity survive restart;
- missing implementation, mismatched format, typed refusal, panic isolation,
  bounded slow-handler pressure, and stale completion can be represented;
- handler code can run outside the store lock and commit through an exact
  source/revision/generation fence.

Exploratory release measurements over nine samples of 100,000 stateless
dispatch iterations—not a backlog of unpublished operations—found
approximately:

```text
direct transform                 221 ns median
registry lookup + dynamic call   244 ns median
lookup/Arc portion                23 ns median
extra transform allocations        0
```

These captured numbers show a small dispatch tax for the prototype. Nanosecond
microbenchmarks remain host- and load-sensitive; they are evidence for
feasibility, not a performance contract.

The second layer routed a genuinely bodyless semantic payload through the real
public Rust `Engine::publish(WriteIntent)` door. That behavior is not the
production target. At measured head
`283132d2617dc5dff2be538e5385385554420140`, it demonstrated historically that:

- semantic acceptance uses the ordinary receipt and intent-id allocators,
  caller-correlation index, and redb database; there is no second public
  acceptance method;
- the returned stable receipt initially has no event id, then names the event
  id installed by materialization; two receipts can share that event id;
- exact ordered operation bytes and both receipts survive close/reopen, and a
  handler registered later can materialize them;
- a source can advance before the first materialization or after an installed
  materialization; the former uses the qualified successor as its base, while
  the latter installs a successor event id under the same receipt;
- handler execution remains outside the serialized engine/store owner, and
  installation compares the exact target, selected source event id, ordered
  operation digest, and generation;
- a deliberately delayed stale completion is observably processed as stale
  before the test inspects current state;
- exactly four handlers execute concurrently, and work deferred behind full
  capacity resumes after success, refusal, or panic; and
- a blocked handler does not block an ordinary publication, and engine
  shutdown does not wait for a native callback that never returns.

Release measurements on an Apple M3 Max used nine fresh-process batches. The
final experiment/report head is
`566b5ef246152267a94728bd31517beceb3156a3`; its raw artifact, SHA-256
`fd350429f85a5a947639dec0174bc761cf09c05cd0f3702da2659a74e80b30b0`,
retains every iteration. The medians and 95th percentiles were:

| Workload | Median | p95 |
|---|---:|---:|
| Ordinary in-memory event acceptance | 66 microseconds | 220 microseconds |
| Bodyless semantic in-memory acceptance | 28 microseconds | 226 microseconds |
| Ordinary redb event acceptance | 5.84 milliseconds | 11.52 milliseconds |
| Bodyless semantic redb acceptance | 10.37 milliseconds | 20.05 milliseconds |
| Ready semantic acceptance through installed body | 78 microseconds | 1.11 milliseconds |
| Reopen until exactly 100 receipts are inspection-ready | 16.1 milliseconds | 48.8 milliseconds |
| Late registration through all 100 reopened redb installs | 1.66 seconds | 1.99 seconds |
| Reopen, register, and install one source-driven successor | 26.7 milliseconds | 29.7 milliseconds |
| One hundred 5-millisecond handler jobs, end to end | 156.8 milliseconds | 179.5 milliseconds |

The slow-handler workload observed exactly four active callbacks. Exact
ordered sequences of 1, 10, and 100 retained operations all passed their
materialization oracle; median registration-to-install time was respectively
18.8, 17.4, and 20.2 milliseconds. Those realistic sizes revealed no practical
preparation cliff that justifies adding paged scheduling now.

The ordinary and bodyless acceptance rows perform different work and end in
different states. They describe the prototype workloads; they are not a claim
that the difference is an architecture tax or regression.

The bodyless acceptance, missing-handler persistence, late registration, and
nullable initial event id in those bullets are retained here only so the
experiment's measurements remain interpretable. The #1412 decision gate
explicitly rejected all four for production.

### 22.9 What the rejected #1412 prototype did not prove

Using the real acceptance door proves the front half of one public lifecycle;
it does not yet prove the ordinary lifecycle after a body is installed. The
experiment stores semantic operation, resource, and receipt projections in
parallel experimental JSON tables inside the same redb database. Sharing a
database and id allocators does not make those records the canonical pending
event, signing obligation, delivery lanes, or terminal receipt state.

The experiment therefore did not prove:

- a production binary schema or migration for semantic operations;
- canonical optimistic query transitions;
- the full source-plan and access-context qualification model described in
  this document;
- atomic replacement of canonical source and effective query state;
- current publish-queue successor delivery;
- signing or routing of an installed semantic materialization;
- event-qualified relay attempts, acknowledgements, retry, or settlement;
- cancellation with shared materializations;
- removal or compaction of semantic receipts and operations;
- complete Rust/FFI/Swift/Kotlin projection;
- arbitrary native handler callbacks;
- hostile or permanently non-returning handlers;
- shutdown while callbacks remain in flight;
- capability-defined normalization and production storage bounds.

Relay ingest now extracts the changed replaceable coordinate and prepares only
that target, but this is code-inspected rather than proven by a two-target
runtime falsifier. Queue inspection is bounded at the public door and uses
cursor-ranged receipt pages in Redb.

Handler execution is bounded, but handler registration and restart recovery
still enumerate every active semantic resource and eagerly clone all ready
jobs before the 32-slot executor defers excess targets. Supporting 100,000
simultaneously unpublished semantic operations is deliberately out of scope:
it is not a product requirement established by this work, and paged or bounded
preparation solely for that hypothetical load would be premature complexity.
The 1, 10, and 100-operation measurements above are the current decision
evidence. Enormous eager backlogs remain a non-blocking scale limitation;
batching should be added only if it is nearly free or measurements of plausible
product workloads reveal a practical problem.

The detached-worker shutdown behavior prevents one non-returning callback from
hanging one engine shutdown, but it cannot pre-empt native code. Repeated
engines could leave stuck worker threads behind. Panic catching also cannot
contain process abort, unsafe memory corruption, hostile CPU use, or hostile
allocation. Native trust/isolation and callback lifetime therefore remain open
architecture decisions.

### 22.10 Production implementation sequence

This is a dependency order, not a promise that each item is already designed:

1. Hard-cut public row signature projection to one `Pending | Signed` owner.
2. Land compact store state for body-complete operations, one current
   materialization, independent receipts, and non-reused generation fences.
3. Define the exact opaque operation and registration contract in a lower
   mechanism crate with no capability dependency.
4. Extend `WriteIntent` and the real acceptance transaction so a configured
   capability produces a complete initial candidate before custody; preserve
   its initial event id while deleting the one-permanently-current-event-id
   assumption.
5. Integrate off-lock materialization and atomic source/effective replacement.
6. Key crypto, signer, route, delivery, and relay facts by exact generation.
7. Add one ordinary replaceable and one addressable capability consumer; keep
   kind `3` as a consumer rather than a special branch.
8. Add restart, fault, stale-result, partial-route, and realistic
   operation-sequence/recovery proofs; add batching only when implementation
   cost is negligible or measured product workloads require it.
9. Project only the capability workflows and truthful receipt facts across
   FFI, Swift, and Kotlin; do not expose raw opaque bytes as an app API.
10. Update README, known gaps, bug-class ledger, feature corpus, supported
    surfaces, and stale architecture documents only after the behavior is
    actually built and proven.

---

## 23. Deliberately unresolved decisions

These questions are material and must remain visible:

1. What exact bounded policy closes a selected source that never becomes
   reachable?
2. Does a product choose one-shot bounded reconciliation, deliberately
   long-lived reconciliation, or both as explicit modes?
3. What public terminal facts describe operations normalized away before any
   wire event exists?
4. What cancellation is safe when one signed materialization serves several
   receipts?
5. Should Swift and Kotlin reach configured materializers only through
   statically packaged fused capability methods, avoiding arbitrary native
   callback and shutdown hazards?
6. How are successor materializer non-return, cancellation, and application
   shutdown bounded without adding a missing-handler receipt lifecycle?
7. Which exact receipt facts distinguish the stable initial accepted event id
   from later current and retired generations without redundant state owners or
   lifecycle booleans?
8. What clock owns an operation's logical time, and how are equal or
   future-skewed local operation times handled without trusting an app-supplied
   event timestamp?
9. How does a source policy distinguish first-resource creation from
   unresolved absence for each capability?
10. Does the final unsigned payload structurally refuse blind replaceable and
    addressable builders, requiring a capability-owned exact or replayable
    operation, while preserving verbatim externally pre-signed publication?

Until those choices have executable evidence, they remain open. The
implementation must not hide them behind generic terms such as “queued,”
“synced,” “published,” or “conflict resolved.”

---

## 24. Review anchors

This document is the behavior-first input to #1387, not its completion. The
implementation and final promotion must remain traceable to:

- #1380 for the complete semantic-operation contract;
- #1381 for loss-preserving structural transforms;
- #1382 for encrypted candidate preparation before custody and exact crypto
  fences;
- #1408 for compact durable operation/materialization state;
- #1432 for body-complete acceptance and optimistic query projection;
- #1433 for complete source-driven successor rematerialization;
- #1434 for generation-qualified signing, routing, and relay evidence;
- #1406 for long-sequence and recovery bounds;
- #1386 for restart, query, rebase, and generation-qualified delivery proof;
- #1387 for final feature, architecture, status, and SDK promotion;
- #1412 for historical registered-materializer evidence and the explicit
  rejection of its bodyless lifecycle; and
- #1414 for this document's completeness and handoff.

No issue closing, README claim, known-gap removal, ledger closure, or supported
surface promotion should cite this specification alone as proof that the
behavior works.
