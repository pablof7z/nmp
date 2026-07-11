# Signer, crypto, and AUTH capabilities

**Status: CURRENT + TARGET.** Local signing and NIP-44 crypto traits are built.
The standard secure-storage provider boundary, NIP-46 reattachment, pinned
per-write signer override, durable `AwaitingSigner`, completed decrypt return
path, and NIP-42 policy/evidence model are target work.

After this chapter you will know what NMP persists, what the platform signer
provider owns, and why a capability is not an app callback in an engine
decision path.

## A capability answers a bounded engine request

NMP may ask a provider to sign a frozen body, decrypt protocol ciphertext, or
answer another typed cryptographic operation. The engine owns when that request
is valid and how the result affects state. The provider owns the operation and
returns a typed result correlated to that request.

This is not a closure that decides routing, admission, ordering, or demand.
Opaque app code never enters those correctness paths. The provider cannot
mutate the store, choose arbitrary relays, or rewrite the frozen body.

## Current signer seam

The Rust signer trait is built, and `LocalKeySigner` can resolve synchronously:

```rust
pub trait SigningCapability {
    fn public_key(&self) -> Option<PublicKey>;
    fn sign(&self, unsigned: UnsignedEvent) -> SignerOp<SignedEvent>;
}
```

`SignerOp` can be ready or pending, which leaves room for a remote operation
without putting an app callback in the reducer. The current Swift SDK accepts
an nsec through `addAccount`, registers the resulting local signer, and
`setActiveAccount` couples that signer to `Reactive(ActivePubkey)`.

That coupling is current implementation truth, not the target authority model.

## Target signer selection

The common path stays small:

```text
publish(draft)                    // signer registered for currentPubkey
publish(draft, as: identityRef)   // explicit exceptional override
```

Most apps never pass a signer with each write. The default follows
`$currentPubkey`; an override supports a podcast key, disposable identity,
hardware key, delegation, or other non-active identity without re-rooting
queries.

Signer choice is resolved and pinned before durable `Accepted`. A later
current-pubkey change cannot redirect the intent. If the matching capability is
missing or temporarily offline, the canonical pending row and receipt remain
`AwaitingSigner(pubkey)` until a matching provider attaches or the app cancels.
Missing NIP-46 connectivity is waiting, not terminal failure.

Every returned signed event must match the frozen body and expected pubkey
exactly and verify cryptographically before it can promote the canonical row
from `Pending(intentId)` to `Signed(signature)`.

## Secret material boundary

The durable Rust event/outbox store persists obligations, expected pubkeys,
frozen unsigned bodies, signatures, and receipt facts. It does **not** persist
raw nsecs or other signing secrets.

Platform SDKs should ship standard signer providers backed by Keychain,
Android Keystore, or the platform's equivalent secure facility. That avoids
forcing every app to hand-roll vault plumbing while leaving product policy in
the app:

- the app owns identity import, removal, backup, labels, and login UX;
- the SDK owns a standard secure provider implementation;
- custom NIP-46, hardware, or memory-only providers may implement the same
  bounded capability seam;
- the engine owns durable obligations and exact result validation.

A memory-only disposable key may vanish. NMP does not silently discard or
re-author its accepted intent; the receipt waits for reattachment or explicit
cancellation.

## Crypto operations

NIP-44 encrypt/decrypt is also a typed capability. It may be implemented by the
same provider that can sign for an identity, but the architectural requirement
is capability locality, not that secret bytes live in the event store.

Decryption produces raw protocol data. Formatting, display names, thread UI,
and plaintext presentation policy remain app-owned. The current local crypto
implementation exists; the end-to-end decrypt-result path into public query
delivery is incomplete.

## AUTH is source/access context

NIP-42 can change what a relay returns, so AUTH state participates in a query's
`AccessContext` and acquisition evidence. It is not a global "active account"
side effect and not a cache-isolation boundary.

The target policy is a closed value supplied by the app, not a callback. When a
relay challenges, NMP either applies the declared policy with the selected AUTH
capability or exposes facts such as `authRequired`, `awaitingSigner`,
`authenticated`, or `rejected`. Ordinary snapshots carry compact source
evidence; diagnostics retains the exact relay, challenge, connection, policy,
and error facts.

AUTH operations do not silently change `$currentPubkey`, the signer pinned to
another write, or literal multi-account queries.

## Status summary

| Surface | Current | Target |
|---|---|---|
| Local signer | Built | Standard provider remains supported |
| Default signer | Coupled to `setActiveAccount` | Signer for `$currentPubkey` |
| Per-write identity override | Not built | Explicit and pinned at acceptance |
| Missing remote signer | Process-local pending/failure behavior | Durable `AwaitingSigner` and reattachment |
| Secret storage | Local signer currently engine-side | Standard platform vault/provider; no raw secret in event/outbox persistence |
| NIP-44 | Crypto trait built | Complete public result path |
| NIP-42 | Transport defers AUTH | Typed policy plus source/access evidence |

---

<!-- nav-footer -->
<sub>← [Offline & sync](19-offline-sync.md) · [Index](README.md) · [Provenance](21-provenance.md) →</sub>
