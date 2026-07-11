# Identity & multi-account: re-root in one line

**Status: BUILT** — `addAccount` / `setActiveAccount` ship on the Swift SDK and the Rust `Handle`; the re-root, teardown-before-activate, and read/write coupling are all live and headlessly tested (`nmp-engine/tests/core_headless.rs`, contract tests 3 & 10).

After this chapter you can add several accounts, switch the active one with a single call, browse read-only with no key at all, and prove on the diagnostics screen that switching away from an account leaves nothing of the old account on the wire.

## Identity is the one input

NMP has two nouns — a live query and a write intent — and exactly **one input** that feeds them: the active identity. There is no session object, no login flow, no "account vector" inside the engine. You state a fact ("the current account is A"), and everything account-shaped downstream — which follows expand, which outboxes are discovered, which key signs — is *derived* by the engine from that one fact.

Concretely, every account-scoped query hangs off a single binding, `Reactive(ActivePubkey)`. When you write `$myFollows` (see *Live queries and the binding grammar*), the `$myFollows` set is a `Derived` binding rooted at that one reactive atom. There is no second place in the engine where "the current account" lives. That single-rooting is not an implementation convenience — it is the structural mechanism behind **bug-class ledger #10 (multi-account desync / cross-account leak)**: because there is only one root, a switch is a *root replacement*, and a root replacement can be made atomic and exactly-once.

## The whole contract: two calls

```swift
import NMP

let nmp = try NMPEngine(config: .init(
    storePath: cachePath,
    indexerRelays: ["wss://relay.damus.io", "wss://purplepag.es"]
))

// 1. Hand the engine a key. It crosses the boundary exactly ONCE and
//    lives engine-side from here on; you get back the hex pubkey.
let alice = try await nmp.addAccount(secretKey: "nsec1…")

// 2. Make it active. This is the ONLY identity verb you call at runtime.
try nmp.setActiveAccount(alice)
```

That is the entire adoption cost of identity. `addAccount` registers a signing capability keyed by its own public key; `setActiveAccount` is what actually roots reads and writes onto it. Registering an account does **not** activate it — the two steps are deliberately separate so a client can pre-load several keys at launch and switch between them instantly, with no re-authentication round trip.

On Rust the same two verbs, shaped for the `Handle`:

```rust
use nmp_signer::LocalKeySigner;

// Register a signing capability; returns the pubkey it was keyed under.
let alice = handle.add_signer(LocalKeySigner::new(alice_keys));

// Re-root reads AND the active signer together.
handle.set_active_account(alice);
```

## `setActiveAccount` re-roots reads *and* the signer, together

This is the load-bearing design decision, and it is worth stating precisely because the naive multi-account bug is exactly the thing it forecloses. In many Nostr clients, "the account I'm reading as" and "the key that signs my next note" are two separate pieces of state that drift apart — you switch your timeline to Bob but a queued reply still goes out under Alice's key. NMP makes that unrepresentable: `set_active_account` moves *both halves in one verb*.

From the engine's own doc comment on the Rust verb:

> Re-root every reactive query AND the active signing capability together onto `pk` … one verb moves both halves so reads and writes can never diverge onto different accounts.

There is no `setReadAccount` and no separate `setSigningKey`. Because reads and writes are re-rooted by the same call, the class of bug where your feed shows one identity while your publishes carry another cannot occur — not because a lint forbids it, but because the API has no seam to express it.

## What "re-root the whole binding graph" means

When you call `setActiveAccount(bob)`, the engine, in order:

1. Resolves the switch through the resolver as a root replacement.
2. **Closes the old account's graph first** — every atom mentioning Alice is torn down in *reverse-of-open* order, exactly once, before anything for Bob opens. This is the "teardown-before-activate" discipline; the demand set for Alice's follows, her outboxes, her mentions, all withdraw from the wire.
3. Opens Bob's graph — `Reactive(ActivePubkey)` now resolves to Bob, so `$myFollows` re-expands to Bob's follows, new outboxes get discovered, new REQs go out.
4. Refreshes every live handle. Each subscription you hold gets a single row delta that "removes everything old, adds everything new" — the same diff mechanism a normal ingest uses, with no special-casing. Your `for await` loop simply sees Alice's rows disappear and Bob's appear.

The ordering in step 2 is what closes the leak: a stale callback from Alice's now-withdrawn subscription has no surviving wire subscription to fire from. Per ledger #10, "a stale account's callbacks have no surviving subscription."

## Read-only browsing needs no key

`setActiveAccount` accepts *any* pubkey, whether or not you ever handed the engine its secret key. This is how you browse someone else's feed, or your own account before the user has entered their key:

```swift
// No addAccount, no key — just view fiatjaf's world read-only.
try nmp.setActiveAccount(
    "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
)
```

Reads re-root exactly as before. A `publish` attempted while active in a read-only state simply terminates with `WriteStatus.failed` (no active signer) — never a crash, never a silent no-op you can't observe (see *Writing: intents, receipts, and the durability guarantee lattice*). Logging out is `setActiveAccount(nil)`: reads re-root onto nothing, writes fail closed.

## The account list is *your* state, not the engine's

Note what is absent above: NMP never hands you a "list of accounts," a "current user" object, or any onboarding UX. The Falsifier app keeps its own `accounts` array and its own notion of which row is active; the only NMP calls its accounts screen makes are `addAccount` and `setActiveAccount`. From its own source:

```swift
func setActive(_ pubkey: String?) {
    try engine.setActiveAccount(pubkey)   // the only NMP call
    activePubkey = pubkey                  // this app's own state
}
```

Labels, avatars, ordering, which account is "primary" — all app-owned. The engine owns only what is *derived* from the active pubkey. This is the identity-as-input rule from the design guidelines made concrete: "The app states a fact; there is no session model, login flow, or account vector in the engine."

## Proving zero cross-account leak

You do not have to trust the re-root — you can watch it. Open a diagnostics stream (see *Diagnostics & debugging*) alongside your feed:

```swift
Task {
    for await snapshot in nmp.observeDiagnostics() {
        for relay in snapshot.relays {
            print(relay.relay, "subs:", relay.wireSubCount,
                  "authors:", relay.authorsServed)
        }
    }
}
```

Switch from Alice to Bob and watch the numbers move. Alice's write relays — the ones carrying her follows' content — drop their subscriptions; Bob's appear. `authorsServed` per relay recomputes to Bob's follow graph. If any relay were still serving Alice's authors after the switch, `authorsServed` would show it, and the exact wire filters would name the stale pubkeys. That is the acceptance test made visible: the leak ledger #10 forbids is one you can falsify on screen in real time.

## Gaps to know

- **Signer breadth.** Only `LocalKeySigner` (local nsec) is built today. NIP-46 remote signers and platform signers plug into the same `SigningCapability` seam but are not yet shipped — see *Capabilities: signer, AUTH policy, encrypt/decrypt*.
- **Re-root cost.** A switch re-expands the full follow graph and re-discovers outboxes; it is not free, but it is bounded by the new account's demand, not the whole store. There is no background pre-warming of inactive accounts — an inactive account carries zero live demand by design (that is the leak fix), so switching back re-discovers from cache.

---

<!-- nav-footer -->
<sub>← [Editing replaceable state safely](15-editing-replaceable.md) · [Index](README.md) · [Relays: outbox & indexers](17-relays.md) →</sub>
