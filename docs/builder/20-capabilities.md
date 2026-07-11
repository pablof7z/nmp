# Capabilities: signer, AUTH policy, encrypt/decrypt

**Status: PARTIAL** — the signing and encrypt/decrypt capabilities are BUILT (`nmp-signer/src/*`, local nsec today); NIP-42 AUTH policy is **PLANNED-shape** in this chapter (the intended design, not yet shipped — the transport currently defers AUTH). Every AUTH example below is clearly marked as such.

After this chapter you will know what a *capability* is (and why it is not a callback), how to plug in a signer, why decryption lives *inside* the engine next to the key, and what the AUTH-policy surface will look like when it lands.

## A capability is something the engine invokes — not a callback into your code

The design rule from the cross-platform contract: **capabilities and policies, not callbacks.** Where an app must influence engine behavior, it supplies a *capability object* (or a declarative *policy value*) at construction/registration time. The engine then invokes that capability *at the right moment* — but it never calls back into your app to make a routing, demand, or ordering decision mid-flight.

The difference is not cosmetic. A callback the engine invokes to decide *where to route* or *what to admit* would be an opaque closure in the decision path — exactly the seam the whole "values in, code after" architecture exists to exclude. A capability is narrower: it answers a *bounded question the engine asks* ("sign this template," "decrypt this ciphertext"), at a *placement the engine chooses* ("at the awaiting-capability stage," "before emitting raw tokens"). You pick *which* capability exists; the engine owns *when* it runs.

## The signer capability

Signing is a capability with two methods:

```rust
pub trait SigningCapability {
    fn public_key(&self) -> Option<PublicKey>;
    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent>;
}
```

`sign` returns a `SignerOp` — a **pollable thunk**, not an `async fn`. It may resolve synchronously (`SignerOp::Ready`) or later (`SignerOp::Pending`), and the engine polls it on its own blocking recv loop. There is no tokio anywhere in `nmp-signer`; this is the D8 "no poll-loop, blocking recv" discipline. A local key resolves instantly; a remote signer (NIP-46) would resolve `Pending` while the round trip happens, without the engine ever spawning an async runtime.

You register a signer once. On Rust:

```rust
use nmp_signer::LocalKeySigner;

let pubkey = handle.add_signer(LocalKeySigner::new(keys));  // keyed by its own public_key()
handle.set_active_account(pubkey);                          // now it signs + roots reads
```

`add_signer` registers the capability keyed by the pubkey it reports; `set_active_account` is what makes it the *active* signer (and re-roots reads onto the same identity — see *Identity & multi-account*). On Swift the same thing is one call:

```swift
let alice = try await nmp.addAccount(secretKey: "nsec1…")  // key crosses ONCE, lives engine-side
try nmp.setActiveAccount(alice)
```

The engine invokes `sign` at the **awaiting-capability stage** of a write: a `WriteIntent` with an unsigned template produces a `RequestSign` effect, the signer resolves it, and the write proceeds to routing. Signing and publishing stay orthogonal — a caller that already holds a signed event supplies it directly and skips the signer entirely. The receipt stream reports `AwaitingCapability` while this is outstanding, so it is observable, not hidden (see *Writing: intents, receipts, and the durability guarantee lattice*).

`LocalKeySigner` also refuses to sign a template whose pubkey does not match its own key — a mismatch means the caller built the template for a different identity, and silently signing it under the wrong key is exactly the class of bug the capability boundary exists to prevent.

## The crypto capability lives *with* the key

Encryption and decryption are a second capability, `CryptoCapability`, and the critical design fact is that it is **co-located with the signer** — the same type holds both, because the *key lives in the engine*:

```rust
pub trait CryptoCapability {
    fn nip44_encrypt(&self, peer: PublicKey, plaintext: &str) -> SignerOp<String>;
    fn nip44_decrypt(&self, peer: PublicKey, ciphertext: &str) -> SignerOp<String>;
}
```

Why co-located, and why engine-side? Because identity-is-input requires it. If decryption lived in your app, your app would need the secret key — and then "the active identity" would be split across the engine and the app, breaking the single-root property that *Identity & multi-account* depends on. So the key crosses the FFI boundary exactly once (in `addAccount`) and never leaves; decryption happens where the key already is.

This is the **ledger #12 amendment (presentation in core)**. Ledger #12 forbids any presentation in the engine — but it scopes that to *unencrypted* content. Encrypted payloads (NIP-17 gift-wrap, private NIP-51 list items) are decrypted by this engine-internal capability and the engine emits the **decrypted raw tokens** — still doing zero presentation. Decryption is a *capability*, not a third noun, and it does not re-introduce formatting: you get raw plaintext tokens out, and your app formats them, exactly as it formats a hex pubkey or a Unix timestamp.

`LocalKeySigner` implements both traits over one `nostr::Keys`, using rust-nostr's `nip44` for the actual crypto — no scratch cryptography. A round trip:

```rust
let ct = alice.nip44_encrypt(bob.public_key(), "gm")?;   // SignerOp::Ready(Ok(ciphertext))
let pt = bob.nip44_decrypt(alice.public_key(), &ct)?;    // "gm"
```

Inside the engine, a decrypt is placed as a `RequestDecrypt(EventId, PublicKey, ciphertext)` effect at the point *before* raw tokens are emitted for a private event — the same "invoke at the right moment" placement as signing.

### Gap: the decrypt feedback path

The `CryptoCapability` trait and `LocalKeySigner`'s impl are built and tested, and the engine emits `RequestDecrypt` at the right seam. What is **not** yet fully wired is the return path that folds a decrypted result back into an emitted row across the FFI surface — so end-to-end decrypted delivery to a Swift app is not shippable today. The capability and its placement are correct; the plumbing that carries the plaintext back to `observe` is the remaining work.

## AUTH policy — the intended shape (PLANNED)

> Everything in this section is **PLANNED-shape**: the intended design, not yet shipped. The transport today defers NIP-42 — the `Closed`/`Notice`/`Auth` frames are parsed but not acted on (a plan §7 non-goal until a falsifier test forces it). Do not write code against this yet.

NIP-42 lets a relay challenge a connection with `AUTH`. The wrong way to handle it is a callback: "engine asks app, mid-subscription, whether to authenticate here" — that is an app closure in the transport decision path, precisely the shape the capability rule rejects. The right way is an **app-injected policy *value*** supplied at construction:

```swift
// PLANNED-shape — illustrative, not a shipping API.
let nmp = try NMPEngine(config: .init(
    indexerRelays: [...],
    authPolicy: .init(
        // Relays the user explicitly added → authenticate automatically.
        autoAuth: .userConfiguredRelays,
        // Any other relay that challenges → surface a prompt decision.
        unknown: .prompt
    )
))
```

The policy is a **declarative value the engine evaluates**, not a function the engine calls. When a relay issues an `AUTH` challenge — including *mid-subscription*, after a REQ is already open — the engine consults the policy value:

- **auto-auth** for relays the user configured themselves (the `UserConfigured` lane from *Relays: outbox, indexers, and roles*): the engine signs the AUTH event via the same signer capability and continues, no app round trip.
- **prompt** for an unknown relay: the engine surfaces the decision as observable state (a diagnostic/receipt-style signal), and the app answers by *updating the policy value*, not by returning from a callback.

The AUTH event itself is an `Ephemeral` write (fire-and-forget, no receipt — see *Offline & sync*), signed by the active signer capability at the transport edge. The key property carried over from the built capabilities: the app chooses *which policy exists*; the engine owns *when and how* AUTH runs. No closure ever enters the routing or admission path.

## Summary of what's real today

| Capability | Status | Notes |
|---|---|---|
| `SigningCapability` (local nsec) | BUILT | `LocalKeySigner`; `SignerOp` pollable thunk, no async |
| `CryptoCapability` (NIP-44 encrypt/decrypt) | BUILT (crypto) | co-located with signer; decrypt *return path* to FFI is the gap |
| NIP-46 remote signer | PLANNED | same `SigningCapability` seam; resolves `SignerOp::Pending` |
| NIP-42 AUTH policy | PLANNED | app-injected declarative policy value, not a callback |

The seams are the durable part; the local implementations are what fill them today, and every future signer/AUTH capability plugs into the *same* traits without widening the public surface.

---

<!-- nav-footer -->
<sub>← [Offline & sync](19-offline-sync.md) · [Index](README.md) · [Provenance](21-provenance.md) →</sub>
