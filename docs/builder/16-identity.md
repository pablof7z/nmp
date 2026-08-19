# Whole-session accounts and current selection

NMP owns one engine session containing unique accounts keyed by decoded public
key, each account's optional persistable provider, and the current selection.
The app owns the product policy and presentation around that session.

## The app owns account product policy

Your app owns:

- labels, ordering, and account-selection UI;
- import, backup, removal, and secure storage policy for the opaque whole-session
  payload;
- whether logout should preserve or destroy the shared local cache.

NMP owns account membership, current selection, provider reconstruction, and
the runtime availability used when a write needs signing.

## Current account has two ergonomic roles

Adding an account and marking it current serves two roles:

1. `Reactive(CurrentPubkey)` bindings re-root to the new value.
2. A write using `Identity::Active` selects that account's signing provider.

Those roles share a default but remain separable.

Changing the current account does not:

- rewrite literal multi-account demands;
- isolate or clear cached rows;
- retarget already accepted writes;
- require a signer to exist for read-only use; or
- make every operation use the new identity.

## Read-only and multi-account demand

A current account may be public-key-only. Its signing capability is
`unsupported`, and read-only browsing remains valid.

An app that watches all of its accounts writes the literal demand it actually
wants: a filter naming the caller-selected kind with a `p` tag binding to the
literal set of all local account pubkeys.

That query stays unchanged when the selected/current account changes. App state
can annotate each row with which local account was tagged.

## Accounts carry optional persistable providers

Ordinary writes should not force the app to pass a signer repeatedly. Adding a
private-key account once as current is enough for a later `publish` call to
resolve a signer automatically.

The current implementation persists and reconstructs the local-key provider.
NMP asks the configured provider to sign one exact frozen body when needed and
validates its result before promotion.

The app exports one opaque session payload and stores it atomically according to
its platform security policy. Raw provider restoration material does not enter
the event/delivery store, snapshots, diagnostics, or logs. Additional provider
implementations must project through the same account/session model.

## Override one write without changing the current account

This supports podcast keys, disposable identities, delegates, hardware keys,
and remote signers as provider implementations are added. The write names a
decoded public key through `Identity::Explicit`; it does not alter reactive
queries rooted at the current account.

NMP resolves and pins the chosen identity at acceptance. A later account switch
cannot redirect the pending intent.

## Provider availability and resumption

Once NMP can resolve the expected author identity, a configured provider being
unavailable does not block durable acceptance into the canonical store and
receipt journal:

```text
accepted(intentId)
awaitingSigner(pubkey)
```

The unsigned pending row remains visible to matching queries. When the
configured provider becomes available, NMP resumes the existing obligation.
For a local-key account, restoring the whole session or adding that decoded
private key again reconstructs the provider.

The app does not recreate the intent or mutate the pending row. Provider
availability is runtime capability state, not permission to discard accepted
data.

## Shared cache trust domain

One engine instance has one canonical cache. Accounts in that engine are not
separate mutually untrusted users. Validated public rows and locally accepted
rows remain available to any local query that matches them.

For a device/app used by mutually untrusted people, logout must be explicit.
`engine.session.clear()` removes accounts, providers, and current selection but
deliberately preserves cached events, receipts, and accepted write obligations.
Destroying the shared local trust domain is a separate destructive-store reset,
performed only after shutting down every engine using that path. An ordinary
current-account change must never pretend to provide either boundary.

## AUTH identity is query context

Relay AUTH may change what a source returns. A demand therefore carries access
context independently of the app's selected account — for example, a
selection paired with a module-minted routing and an explicit AUTH identity.

The protocol module mints `group.readRouting` from typed NIP-29 group
context. The app may supply the public group host through that semantic
constructor, but cannot convert a relay URL into generic authority for unrelated
demand.

Evidence from one AUTH identity cannot prove acquisition for another. The app
still owns whether and when that identity is acceptable for product policy.

---

<sub>[Index](README.md) · Related: [Writing and receipts](14-writing.md) · [Source and routing context](17-relays.md)</sub>
