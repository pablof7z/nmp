---
title: The event builder
category: writes
slug: event-builder
status: built
date: 2026-07-29
owns:
  - what replaces `WritePayload::Unsigned`, and why neither "unsigned" nor "draft" survives
  - the builder's shape, and why it is a value rather than an object
  - why an authorless builder is strictly stronger than #47's mismatch check
  - the builder's Rust and FFI projections, and why a UniFFI Object builder was rejected
  - the `nostr::EventBuilder` naming collision and its resolution
  - the death of the deterministic-bytes requirement
related:
  - docs/internals/writes/identity.md
  - docs/internals/routing/auto-and-explicit.md
  - docs/internals/routing/resolution-lifecycle.md
  - docs/internals/routing/knowledge-and-settlement.md
  - docs/internals/routing/outbox.md
  - docs/internals/routing/resolvers.md
  - docs/internals/routing/preview-and-observability.md
  - docs/internals/nip29/group-publication.md
issues:
  - "#47 identity override — its fail-closed mismatch check becomes structurally unnecessary on this path"
  - "#838 removed NIP-29's author-deriving composer; this is the universal home for that capability"
---

# The event builder

This is the design record for the type that replaces `WritePayload::Unsigned`:
how an app says "here is roughly the event I want" and has NMP fill in what is
missing — `created_at`, `pubkey`, `id`, `sig` — while the engine uses the
current account's provider or an explicit identity's provider. It was settled
in the 2026-07-28/29 design
session with Pablo (repository owner); quotes are his, verbatim.

The one-sentence summary: **the builder is a value that demands a kind, can
express anything else, and structurally cannot carry an author** — and that
last property is what makes the whole design safe.

`status: built` — this document preserves the pre-implementation design record.
Its §1 source references describe the tree immediately before #1005; the
current implementation and current-tree anchors are recorded in §8 rather than
rewriting the alternatives and reasoning that led to it.

---

## 1. What existed immediately before #1005, and the gap — HISTORICAL

`WritePayload::Unsigned(UnsignedEvent)` (`crates/nmp-grammar/src/write.rs:34`)
is the template path: the engine freezes the body at acceptance and signs it
("the key lives in the engine"; signing and publishing are orthogonal stages,
VISION P). `WriteIntent.identity_override: Option<PublicKey>` (#47) selects
which key: `None` means the current account, `Some(pk)` is explicit per-write
consent.

The gap is that `nostr::UnsignedEvent` **requires** `pubkey` and `created_at`
— `UnsignedEvent::new` takes exactly those (noted at
`crates/nmp/src/lib.rs:285-288`). And the engine deliberately never fills
either in: with `identity_override: Some(pk)`, `pk` must EQUAL the draft's
author or the write fails closed pre-acceptance
(`crates/nmp/src/core/write.rs:1590-1605`); with `None`, the draft's author
must equal the current account (`write.rs:1607-1628`). The doc on
`on_publish` states the invariant this enforces: "the reducer never restamps a
draft; a mismatch fails closed with no `Accepted`" (`write.rs:1469`).

So *both* halves of "just publish this as me / as this signer" are blocked by
the same requirement: the convenience case forces the app to know its own
pubkey before composing, and the explicit-signer case forces it to know that
pubkey and stamp it into the draft first. The failure is universal — every
kind, every NIP — which is why it must not be solved inside any protocol
crate. #838 removed NIP-29's `groupMessageIntent` (which derived author and
time internally) for exactly that reason: identity policy did not belong
there. The universal write plane is the correct home for the capability #838
deleted.

## 2. The ruling: an event builder, not an unsigned, not a draft — DESIGNED

Pablo, rejecting both the current name and the candidate replacement:

> I dont think unsigned should exist nor draft; I think "event builder" is a better name for what this is?

On what it must require:

> it should demand the kind

And on what it must be able to express:

> obviously nmp needs to allow apps to set the created_at..., arbitrary tags, it should be able to provide ANYTHING.

The type:

```rust
pub struct EventBuilder {
    pub kind: Kind,                       // the ONE constructor argument
    pub tags: Vec<Tag>,                   // caller-owned, arbitrary
    pub content: String,
    pub created_at: Option<Timestamp>,    // None → stamped at acceptance
}
```

`Kind` is the only thing a builder cannot exist without. `created_at`,
`pubkey`, `id`, and `sig` are filled by NMP when absent — but "NMP fills what
you didn't say" does not mean "you can't say it": `created_at` stays settable,
tags are arbitrary, and nothing is validated against a whitelist of kinds. An
app that wants to hand-roll its own gift wrap can:

> are you saying that an app could hand roll their own giftwrap and yolo it? Yes, of course! preventing that would require making nmp impossible to work with! if a developer wants to shoot themselves on the foot we need to let them do that, we can provide guardrails, but at a certain point we'd be introducing more harm by adding restrictions than not.

Two fields are deliberately NOT expressible on a builder: `id` and `sig`.
They are derived from the signed bytes and can only be meaningful on a payload
that already went through a signer — which is what `WritePayload::Signed`
exists for. A caller holding a signed event uses that path; a builder is by
definition the pre-signature half of the lifecycle.

`Unsigned` and the word "draft" are deleted in the same change that lands the
builder. No alias, no deprecation window, no second way to say one thing.

## 3. The load-bearing decision: the builder is a VALUE — DESIGNED

This is the single most important sentence in this document: **`EventBuilder`
is a plain data value, not an object with methods and identity, and it
structurally cannot carry an author.**

That one decision resolves the two hardest problems in this design at once.

**First, the identity-safety question.** The design exploration's biggest
recorded risk was silent identity erosion: #47's guarantee is "the engine
never restamps a draft", and that guarantee is *enforced by comparison* — a
stated author that disagrees with the override fails closed. If `pubkey`
simply became optional on `UnsignedEvent`, the check would have nothing to
compare and "never restamps" would silently decay into "engine picks". The
builder dissolves this instead of mitigating it: there is no `pubkey` field,
so the author/override mismatch class is **unrepresentable**, not fail-closed.
A caller cannot state an author on a builder, so there is no second source of
truth for the identity to disagree with. #47's "the engine never restamps a
draft" becomes "there is nothing to restamp" — which is strictly stronger than
the check it replaces, because a class of writes that previously *failed
correctly* now cannot be constructed at all. (The check itself survives on the
one payload that still states an author; see `writes/identity.md` §4.)

The same structural argument covers `created_at`: absent-then-stamped is fine;
present-then-changed stays impossible because the engine fills the field only
when it is `None`. A builder that states a timestamp keeps it, verbatim, in
the frozen body.

**Second, the UniFFI constraint** — see §4.

What the value-ness rules out is as important as what it enables. The builder
carries no engine reference, no session, no signer handle. Composing one is
pure and infallible; everything that can fail — no current account, no
available signing provider, a stale replaceable base — fails at
`Engine::publish`'s
acceptance boundary, where #47's machinery already lives. There is exactly one
publish door, and the builder is an argument to it, not a second lifecycle.
(#838 deleted `publish_composed` precisely to avoid two write paths; a builder
that owned its own `send()` would recreate that mistake.)

## 4. Cross-platform projection — DESIGNED

The builder projects differently per platform, because the idiomatic spelling
of "a record with defaults" differs per platform — but it is the same value
everywhere.

**Rust** gets consuming combinators:

```rust
EventBuilder::new(Kind::TextNote)
    .content("hello")
    .tag(tag)
    .created_at(ts)   // optional; omit to be stamped at acceptance
```

Each method takes `self` and returns `Self`. No interior mutability, no
builder-of-a-builder, and — because the struct's fields are public — a caller
that prefers struct literal syntax can use it directly.

**FFI** gets a UniFFI `Record` with `#[uniffi(default)]` fields. Chained
methods returning `Self` project badly through UniFFI (this constraint had
already eliminated designs in the NIP-29 thread), but that is not a loss to
route around — it is a signpost: Swift and Kotlin's *native* idiom for a
defaulted builder IS a labeled-argument initializer. A Swift caller writes
`EventBuilder(kind: 1, content: "hello")` and a Kotlin caller
`EventBuilder(kind = 1u, content = "hello")`, with `tags` and `createdAt`
defaulted. This is already the established pattern on this exact surface:
`FfiWriteIntent` defaults both `identity_override` and `correlation` with
`#[uniffi(default = None)]` (`crates/nmp-ffi/src/types.rs:626` and `:638`), so
existing callers construct intents with labeled arguments and omit what they
don't use.

**Why a UniFFI *Object* builder was rejected.** UniFFI objects cross the
boundary as `Arc`-wrapped handles; a fluent method on one cannot consume
`self`, so every combinator forces interior mutability (a lock or cell) plus a
foreign-to-Rust round-trip per field set, plus a terminal `build()` call to
get the value back out — three costs, each of them pure overhead for a type
with no identity and no lifetime. Objects are for long-lived handles *with*
identity (an engine, a stream, a receipt handle). A builder is four fields of
data. Making it an object would spend the expensive representation on the
cheap thing.

The failure mode this split avoids is worth naming: if Rust and FFI shared one
representation chosen for FFI's benefit, Rust callers would get a field-bag
with no combinators; chosen for Rust's benefit, FFI callers would get the
Arc-and-mutex object. Divergent *idioms* over one identical *value* is the
design; the record's fields and the combinators' results are the same four
fields, and both feed the same `WritePayload::Event` variant.

## 5. The naming collision, and why apps never see it — DESIGNED

`nostr::EventBuilder` already exists, and NMP's core imports it today:
`crates/nmp/src/core/mod.rs:58` pulls `EventBuilder` from `nostr` alongside
`Event as SignedEvent`. The preferred name is taken — internally.

But the collision stops at the crate boundary. The facade's re-export of
nostr value types is exactly:

```rust
pub use nostr::{Event, EventId, Kind, PublicKey, RelayUrl, Tag, Timestamp, UnsignedEvent};
```

(`crates/nmp/src/lib.rs:288`) — `nostr::EventBuilder` is NOT re-exported, and
never was. So the collision is internal-only: alias the upstream import where
core uses it (the existing `Event as SignedEvent` on the same import line is
the established precedent for exactly this move), export NMP's own
`EventBuilder` from the facade, and **apps see exactly one `EventBuilder`**.
No app ever has to write a disambiguating path, and the name Pablo chose is
the name apps get.

**Correction, recorded while building this (#973/PR #1005).** This section
originally said `UnsignedEvent` leaves that re-export list when `Unsigned`
dies. It does not, and the reason it gave — that being `Unsigned`'s argument
was its only public job — is false on master. `Handle::sign_event(UnsignedEvent)`
(#464, `crates/nmp/src/runtime/mod.rs`) is a public door: governed sign-only,
deliberately separate from publication, with no write intent, pending row,
receipt, or route. Its parameter has to be nameable, so the re-export stays.
This is not a compatibility alias — nothing forwards to a deleted spelling —
it is one live public API's argument type. Removing it means giving
`sign_event` an argument type of its own first, which is a separate decision
about the signing surface and was out of scope here.

## 6. The killed requirement: deterministic bytes — DESIGNED

There was exactly one argument for keeping `Unsigned` alive alongside a
builder: reproducible composition. If composing the same logical event twice
must produce byte-identical output, then `created_at` (and the author) must be
explicit caller inputs, because a stamp applied at acceptance time makes two
compositions differ. `nmp-nip22` is the crate that embodies this position
today: `comment_intent` (`crates/nmp-nip22/src/intent.rs:23`) takes explicit
`author: PublicKey` and `created_at: Timestamp` parameters, and its module doc
records the choice as deliberate — "`author` and `created_at` are explicit
caller-supplied parameters (this issue's own design decision): no
current-account query, no wall-clock read, hence zero engine dependency for
this whole crate" (`intent.rs:4-6`).

Pablo's ruling, in full:

> nip22 comment to prevent twice producing the same identical bytes; that check is absolutely stupid and shouldn't exist -- if it did, it wouldn't be a nip22 concern it would be a concern of any event -- and its not a concern -- we don't need that shit at all. -- so "the cost of folding" is non-existent.

Note the shape of the argument, because it generalizes: if reproducible bytes
were a real requirement it could not live in one NIP's composer — it would be
a property of every event, enforced universally. It is enforced nowhere,
wanted nowhere, and needed nowhere. So it is not a requirement at all.

Two consequences:

- **`nip22::comment_intent`'s explicit `author`/`created_at` parameters lose
  their justification.** The precise accounting: the *purity* half of the
  code's own stated rationale — no current-account query, no wall-clock read,
  zero engine dependency — survives trivially under a builder, because a
  composer returning an `EventBuilder` still reads no clock and queries no
  account; the ENGINE stamps at acceptance. The only justification that
  actually *required* those parameters — that the composer's output be fully
  determined by its inputs, byte for byte — is the one Pablo killed. With it
  dead, the parameters are pure caller tax.
- **With it dies the only argument for keeping `Unsigned` alongside the
  builder.** Every other capability `Unsigned` has, the builder has (state
  anything, including `created_at` — §2), or the pre-signed path has (`id`,
  `sig`). Reproducible composition was the residue, and it is now nothing. So
  "the cost of folding" `Unsigned` into the builder is, in Pablo's words,
  non-existent.

## 7. What to watch when building this — DESIGNED

- **The builder must stay grammar-level** (`nmp-grammar`, where `WritePayload`
  lives — `crates/nmp-grammar/src/write.rs:1-13` records why: protocol modules
  composing intents must not gain an engine dependency). Schema-only composers
  return `EventBuilder`; composers that own write policy return `WriteIntent`.
- **Stamping is acceptance-time, not compose-time**, and it is the same
  acceptance transaction that resolves identity (`writes/identity.md`).
- **Do not add validation to the builder.** Ruling: it "should be able to
  provide ANYTHING." Guardrails belong in composers and diagnostics, not as
  refusals in the one universal type. The failure mode is well-intentioned
  kind- or tag-shape checks accreting here until hand-rolling a gift wrap
  becomes impossible — the exact outcome Pablo forbade.

---

## 8. Implementation correction — BUILT (#973 / PR #1005)

PR #1005 implemented the design above without retaining a compatibility path:
`WritePayload::Unsigned`, `UnsignedReplaceableEdit`, and the FFI/native
`.unsigned` spellings are deleted. The dated `DESIGNED` labels in §§2–7 are
kept as the decision record; this section is the present-tense correction.

Current source anchors on this revision:

- `crates/nmp-grammar/src/write.rs:49-130` defines the public-field,
  authorless `EventBuilder` and the exact three `WritePayload` variants.
- `crates/nmp/src/core/write.rs:1912-1986` keeps a caller-stated timestamp,
  selects the only author from `Identity::{Active, Explicit}`, and freezes the
  builder at acceptance. There is no author field to compare or restamp.
- `crates/nmp-ffi/src/types.rs:570-638` projects `FfiEventBuilder` as the
  defaulted UniFFI record and `FfiWritePayload::{Event, Signed}` as the whole
  FFI payload surface.
- `Packages/NMP/Sources/NMP/WriteIntent.swift:60-104` and
  `Packages/NMPKotlin/src/main/kotlin/com/nmp/sdk/WriteIntent.kt:70-120` expose
  the corresponding native value shapes, with no author on the builder.
- `crates/nmp-nip22/src/intent.rs:1-39` demonstrates the protocol-module
  consequence: composition stays engine-free and clock-free while returning a
  builder-backed ordinary `WriteIntent`; identity and time resolve at the
  engine acceptance boundary.

The rejected alternative from §1 remains load-bearing: author/time derivation
must not move into one protocol crate. The universal write plane owns those
facts, so a future protocol composer returns an `EventBuilder` or a closed
`WriteIntent` and never recreates the removed author-bearing draft shape.
