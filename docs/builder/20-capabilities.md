# Signer, crypto, and AUTH capabilities

A capability answers one bounded engine request. It does not become arbitrary
app code inside routing, demand, persistence, or admission.

## Signer provider

The common path registers a provider for an identity and lets current pubkey
select it by default:

```swift
try engine.attachSigner(provider, for: pubkey)
let receipt = try engine.publish(.init(draft: draft, durability: .durable))
```

A per-write identity override selects another registered provider without
changing current pubkey.

At acceptance NMP freezes the body, expected pubkey, final id, and chosen
identity reference. The provider receives exactly that signing request.

## Sign without publishing

Browser/NIP-07 hosts sometimes need to authorize an external client's exact
event while retaining origin and publication policy themselves. Use the
engine's sign-only operation for that case:

- Rust: `Engine::sign_event(unsigned)` returns a cancellable operation.
- Swift: `try await engine.signEvent(unsigned)`.
- Kotlin: `engine.signEvent(unsigned)` from a coroutine.

The unsigned value names the active account explicitly. NMP refuses a missing
active signer or author mismatch, freezes the exact body, routes only to the
registered capability, and validates the returned body, author, id, and
signature. Success returns a signed event value only. It does not create a
write intent, canonical row, receipt, delivery work, relay plan, or publication.

Origin allowlists, user prompts, and browser networking are app policy. If the
host later decides to publish, it submits the already-signed event through the
ordinary governed write path as a separate action.

## Provider output is untrusted input

A provider result must:

- contain the identical frozen kind, tags, content, created-at time, pubkey,
  and id;
- carry a cryptographically valid signature for the expected pubkey; and
- correlate to the one outstanding request.

NMP verifies those properties before promoting the canonical row or routing the
event. A provider cannot substitute another valid event or return a forged one.

This rule applies equally to local, remote, hardware, and app-defined providers.

## Missing provider is receipt state

Once expected author identity is resolved, temporary provider absence does not
reject a durable intent. The canonical row remains:

```text
signature = Pending
```

The receipt reports:

```text
awaitingSigner(pubkey)
```

Attaching a matching provider resumes the obligation. The row itself does not
become `AwaitingSigner`; that is a receipt/capability fact.

## Secret material boundary

The durable event/delivery store persists obligations, identity references,
frozen bodies, validated signatures, and receipt facts. It does not persist raw
nsecs, bunker credentials, hardware secrets, or bearer tokens.

Platform SDKs provide standard Keychain/Keystore-backed providers. The app owns
identity import, removal, backup, labels, and login policy and may supply custom
remote/hardware/memory providers.

For a personal or development app that explicitly accepts plaintext sandbox
storage, Swift and Kotlin also expose `NMPInsecureFileAccountStore`. Passing it
to engine construction makes `addAccount` checkpoint the validated key and
reattach it on the next construction. The checkpoint is separate from the
canonical event/delivery store, never appears in snapshots or diagnostics, and
must be cleared before engine shutdown to sign out. Its name is literal: it is
not Keychain, Keystore, encrypted, hardware-backed, or a substitute for the
standard secure providers tracked by #47.

A memory-only key may disappear. NMP does not re-author or silently discard its
accepted intent; the receipt waits for equivalent provider reattachment or
explicit cancellation/terminal policy.

## Encrypt and decrypt

Private protocols may request typed encrypt/decrypt operations from the provider
owning the identity. Core or the exact protocol module validates where the
result belongs.

Decryption yields protocol data, not presentation. The app owns formatting,
labels, thread UI, notifications, and plaintext display policy. Sensitive
payloads never appear in diagnostics or replay logs.

## Relay AUTH

NIP-42 can change one relay's answer, so AUTH is part of a demand's access
context. A protocol/operator policy selects an identity reference as a closed
value; an app callback does not decide per frame.

Diagnostics retains challenge, connection generation, identity/policy
reference, response result, and error without exposing secrets. Ordinary query
snapshots receive compact facts such as AUTH required, awaiting capability,
authenticated, or rejected.

AUTH never silently changes current pubkey, retargets another write, partitions
the shared cache, or grants protocol-host authority to an arbitrary relay.

## Retry ownership

- One signer request is owned by the provider adapter and correlated once.
- Provider connection/AUTH recovery belongs to that adapter.
- Durable delivery owns publication attempts after signing.
- The engine's one deadline scheduler owns wakeups and concurrency.

No layer starts a polling timer or secretly buffers another layer's durable
obligation.

---

<sub>[Index](README.md) · Related: [Identity and signers](16-identity.md) · [Writing and receipts](14-writing.md) · [Provenance and private authority](21-provenance.md)</sub>
