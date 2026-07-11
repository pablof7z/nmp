# Writing: intents, receipts, and the durability guarantee lattice

**Status: BUILT** (Swift + Rust SDKs; the receipt stream is live-proven against real relays. Pre-signed publish across the FFI is PARTIAL — see the aside at the end.)

After this chapter you'll be able to publish an event and *know what happened to it* — not "did the call return," but "which relays accepted it, which rejected it, and which never answered." You'll know how to pick a write's **durability class**, why a durable publish never hands you a `bool`, and why the honest answer to "is it sent?" is a stream, not a return value.

Writing is the second of NMP's two nouns. A **write intent** is a plain value — an unsigned template plus two typed properties (durability, routing) — that you hand to `publish`. The engine signs it (with your identity's key, engine-side), routes it through the same lane machinery reads use, sends it, and reports every per-relay outcome back to you on a **receipt stream**. You never name a relay.

## The intent is a value; the receipt is a stream

Here is the whole shape in Swift. Publish a text note as the active account, then render its progress:

```swift
let intent = WriteIntent(
    pubkey: me,                       // the active account (see Identity chapter)
    createdAt: UInt64(Date().timeIntervalSince1970),
    kind: 1,                          // NIP-01 text note — a protocol fact you fill in
    tags: [],
    content: "hello from NMP",
    durability: .durable,
    routing: .authorOutbox
)

let receipt = try await nmp.publish(intent)
for await status in receipt.status {
    switch status {
    case .accepted:                   print("in the outbox")
    case .signed(let id):             print("signed as \(id)")
    case .routed(let relays):         print("routing to \(relays.count) relays")
    case .sent(let relay):            print("sent → \(relay)")
    case .acked(let relay):           print("✓ \(relay)")
    case .rejected(let relay, let r): print("✗ \(relay): \(r)")
    case .gaveUp(let relay):          print("… \(relay) never answered")
    case .failed(let reason):         print("whole write failed: \(reason)")
    case .awaitingCapability:         print("waiting on the signer")
    }
}
```

Note what `publish` returns: a `Receipt` whose `status` is an `AsyncStream<WriteStatus>`. It is **not** a `Bool`, not a `Void`, not "the event id and you're done." This is [bug-class ledger #9](../bug-class-ledger.md) — *enqueue treated as converged* — expressed as a type. `publish` returns the instant the intent is accepted into the outbox; everything that matters happens *after* that, and the only place to observe it is the stream.

The same publish in Rust, over the `Handle`:

```rust
let rx = handle.publish(WriteIntent {
    payload: WritePayload::Unsigned(unsigned),   // kind:1 template, built by you
    durability: Durability::Durable,
    routing: WriteRouting::AuthorOutbox,
});

// rx: Receiver<WriteStatus>. The FIRST value is always Accepted, never terminal.
while let Ok(status) = rx.recv() {
    match status {
        WriteStatus::Acked(relay)        => println!("✓ {relay}"),
        WriteStatus::Rejected(relay, r)  => println!("✗ {relay}: {r}"),
        WriteStatus::Failed(reason)      => { eprintln!("failed: {reason}"); break }
        other                            => println!("{other:?}"),
    }
}
```

`#[must_use]` sits on `Handle::publish` for exactly this reason: dropping the receiver on the floor is how you re-introduce ledger #9. The stream is the contract.

## The status lattice: accepted → signed → routed → sent → per-relay terminal

Every state a durable write can reach:

| Status | Meaning | Terminal? |
|---|---|---|
| `accepted` | In the outbox. Always the **first** state, never the last. | no |
| `awaitingCapability` | Parked on the signer (e.g. a remote NIP-46 signer round trip). | no |
| `signed(eventId)` | The engine's signer capability produced a signed event. | no |
| `routed(relays)` | The router resolved a concrete relay set from lane facts. | no |
| `sent(relay)` | The event left for `relay`. One per routed relay. | no |
| `acked(relay)` | `relay` replied `OK: true`. | per-relay |
| `rejected(relay, reason)` | `relay` replied `OK: false` (e.g. rate-limited, paid relay). | per-relay |
| `gaveUp(relay)` | `relay` disconnected before ever acking. Not a retry — a terminal fact. | per-relay |
| `failed(reason)` | Whole-intent terminal reached **before any relay was contacted**: a signer rejection, or an unroutable private route. No `relay` because none was reached. | whole-intent |

The load-bearing distinction is **per-relay terminals vs. whole-intent terminal**. A durable publish to three relays can end with two `acked` and one `rejected` — there is no single "success." The stream finishes only when every routed relay has reached a per-relay terminal (or the whole intent `failed` up front). That is why "is it sent?" is not a yes/no question: to a *set* of independent relays, it is a set of independent answers, and your UI decides what threshold ("at least one ack") counts as delivered *for your product*.

This is proven live, not asserted: the engine's capstone test publishes one durable intent to two real relays where one accepts and one is configured to reject, and asserts the stream's first state is `accepted` (never terminal) and that the two relays resolve to **distinct** terminals — `Acked` for one, `Rejected` for the other. "Sent" is knowable only by reading the stream.

## The durability lattice: `durable | ephemeral | at-most-once`

Durability is a **typed property of the intent**, not a second write noun and not a routing choice. It answers one question: *what does the engine owe you after `accepted`?*

```rust
pub enum Durability { Durable, Ephemeral, AtMostOnce }
```

- **`durable`** — the full guarantee above. You get the complete receipt stream with per-relay terminals. Use it for anything a user would be upset to silently lose: notes, reactions, contact-list edits, zaps, profile updates. If you're unsure, this is the default.

- **`ephemeral`** — fire-and-forget. The engine still signs, routes, and sends, but **tracks no acks and gives you no receipt** — the stream simply never yields. Use it for state that is worthless a second later: typing indicators, presence, NIP-42 AUTH responses, cursor positions. Waiting for an ack on a typing indicator is wasted machinery; the type says so. Concretely, an ephemeral intent gets no sink at all inside the engine — after `sent`, the pending write is forgotten. **Do not `for await` an ephemeral receipt expecting terminals; you will wait forever.**

- **`at-most-once`** — idempotent RPC. You *do* get the receipt stream (so you can observe outcome), but the engine performs **no blind retry** on disconnect: a `gaveUp` is final, never resent. Use it where a resend would be *wrong* — a NWC "pay this invoice" command, any request whose duplication has real-world cost. The distinction from `durable` is not the receipt (both track per-relay state) but the retry contract: at-most-once forbids the engine from ever re-sending on your behalf.

Choosing:

| You are writing… | Class | Why |
|---|---|---|
| A note, reaction, profile, list edit | `durable` | Loss is user-visible; you want per-relay confirmation. |
| A typing / presence / AUTH signal | `ephemeral` | Worthless if late; don't pay for ack tracking. |
| A payment or idempotent command | `atMostOnce` | Observe the outcome, but never risk a double-send. |

Two honesty notes the type enforces. First, **when NOT to expect a receipt**: `ephemeral` writes yield nothing — that's the contract, not a bug. Second, even the current `durable` class tracks per-relay state for *accuracy of the receipt stream*; automatic relay resend is not yet implemented (a `gaveUp` stays given-up). The stream tells you the truth about what reached each relay; it does not silently paper over a dead relay. If your product needs delivery to N relays, read the stream and act on it — the engine hands you the facts, your app owns the policy.

## Routing: you still never pick relays

`WriteRouting` has exactly three cases, and none of them is a relay list you assemble:

- **`.authorOutbox`** — the author's own NIP-65 write relays, resolved by the same self-bootstrapping outbox the read path uses. This is what you want for public content.
- **`.toInboxes([pubkey])`** — the *recipients'* inboxes (kind:10050 / NIP-65 read relays). For addressed writes.
- **`.privateNarrow([relay])`** — a fixed, **fail-closed** narrow set for private events. This one carries an explicit relay set, but it is *narrow-only*: the type has no widen operation. An empty set is exactly how "unroutable" is expressed, and the engine fails it **closed** — a whole-intent `failed`, zero relays contacted — rather than leaking a private event to a public relay ([ledger #6](../bug-class-ledger.md)). See *Provenance, and why private events can't be republished*.

`.authorOutbox` and `.toInboxes` never let you name a relay at all; the router derives the set from lane facts and reports it back to you as `routed(relays)`. There is no `relays:` parameter on the write path any more than on the read path.

## Where the recipes live (modularity)

You'll notice the examples above fill in `kind: 1` and build tags by hand. That's deliberate. NMP core ships the write *mechanism* — the intent value, the durability lattice, the receipt stream — but **not** a catalog of per-NIP templates. A `.textNote(content:)`, `.reaction(to:content:)`, or `.contactList(...)` recipe is a *protocol fact* (it fills in a kind and a tag shape per its NIP), and under NMP's modularity principle each such recipe ships in its **own opt-in NIP module**, not in core. An app that never reacts links zero reaction code; enabling the reactions module is what makes `.reaction(to:)` appear. Every recipe is a pure function returning a `WriteIntent` value you could have built by hand — it prints its own expansion, and it can never reach a relay your hand-built intent couldn't. The recipe layer is convenience over the mechanism this chapter describes, never a gate in front of it. See *The batteries: recipes catalog* and *Extending NMP: protocol modules & recipes*.

## Presenting in-flight status is your job

The engine emits states; it never renders them. The `switch` in the first example — mapping `acked` to a checkmark, `gaveUp` to a spinner-that-stops, choosing that one `acked` is "delivered enough" to dismiss the composer — is all app code, exactly as it should be. A reasonable SwiftUI pattern is to fold the receipt stream into a small per-message `@Observable` (`sending / delivered(count) / failed`), the same way you fold a read query's rows into view state. The engine gives you the raw progression; the meaning you assign to it is your product.

---

**PARTIAL aside — pre-signed publish across the FFI.** The Rust core already accepts a `WritePayload::Signed` (a pre-signed event skips `RequestSign` and goes straight to routing — this is what makes signing and publishing *orthogonal*, and it's what lets a private event be re-routed to a recomputed narrow relay set without re-signing). The ergonomic SDKs currently expose only the `Unsigned` path (`WriteIntent` carries a template; the engine signs). If you need to publish an already-signed event from Swift today, that seam isn't surfaced yet — track it against the write-path status in [`README.md`](../../README.md). The durability and routing story above is fully built; only the *pre-signed entry point* is pending at the FFI.

---

<!-- nav-footer -->
<sub>← [Delivery-side transforms](13-delivery-transforms.md) · [Index](README.md) · [Editing replaceable state safely](15-editing-replaceable.md) →</sub>
