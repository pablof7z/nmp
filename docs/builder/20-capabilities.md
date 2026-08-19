# Signer, crypto, and AUTH capabilities

A capability answers one bounded engine request. It does not become arbitrary
app code inside routing, demand, persistence, or admission.

## Session account and signer provider

One whole session owns every account, its optional persistable signer provider,
and the current selection. A public-key-only account is a valid account whose
signing capability is `unsupported`; a local-private-key account has the
`localKey` provider and reports its current operational availability. For
example, Swift adds and selects a decoded private key in one transition:

```swift
let account = try engine.session.add(privateKey: privateKey, makeCurrent: true)
let receipt = try engine.publish(intent)
```

A per-write explicit identity selects another session account without changing
the current account.

At acceptance NMP freezes the body, expected pubkey, final id, and chosen
identity reference. The provider receives exactly that signing request.

## Sign without publishing

Browser/NIP-07 hosts sometimes need to authorize an external client's exact
event while retaining origin and publication policy themselves. Use the
engine's sign-only operation for that case:

- Rust: `Engine::sign_event(unsigned)` returns a cancellable operation.
- Swift: `try await engine.signEvent(unsigned)`.
- Kotlin: `engine.signEvent(unsigned)` from a coroutine.

The unsigned value names its author explicitly. NMP requires that author to be
the current account with an available signing provider, freezes the exact body,
routes only to that provider, and validates the returned body, author, id, and
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

## Provider unavailability is receipt state

Once expected author identity is resolved, a configured provider being
currently unavailable does not reject a durable intent. This is distinct from
a public-key-only account, for which signing is unsupported. The canonical row
remains:

```text
signature = Pending
```

The receipt reports:

```text
awaitingSigner(pubkey)
```

Adding the matching private-key-backed account resumes the obligation. The row
itself does not become `AwaitingSigner`; that is a receipt/capability fact.

## Secret material boundary

The durable event/delivery store persists obligations, identity references,
frozen bodies, validated signatures, and receipt facts. It does not persist raw
nsecs, bunker credentials, hardware secrets, or bearer tokens.

The app persists one opaque `SessionPayload` as a whole and supplies it when it
constructs the next engine. The payload may contain provider restoration
material and is sensitive: store it atomically in platform-appropriate secure
storage, do not parse or partially edit it, and never log it. Session restore
is all-or-nothing. Provider availability is live runtime state, not persisted
truth.

Clearing or removing session accounts does not delete cached events, receipts,
or accepted write obligations. NMP does not re-author or silently discard an
accepted intent; its receipt waits for a matching provider to become available
again or for explicit cancellation/terminal policy.

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

Diagnostics is SHAPED to retain challenge, connection generation,
identity/policy reference, response result and error without exposing secrets.
It is populated on both the write path and the read path (#1889): a protected
read transmits its request, the relay's challenge answers it, and the session's
row carries the real epoch, challenge descriptor, policy/signer binding and
signed AUTH event id. The hardcoded `AwaitingChallenge` row is only what a
protected session reads as between connecting and its first challenge.

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
