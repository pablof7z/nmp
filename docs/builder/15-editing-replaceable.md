# Editing replaceable state safely

**Status: IMPLEMENTED CONTRACT.** NMP accepts protocol-owned semantic
operations, materializes them over its best canonical source, and retains them
for replay when newer source truth arrives. The first public operation is
NIP-02 following (`nmp.follow(pubkey)` / `nmp.unfollow(pubkey)`).

## The destructive-write trap

Profiles, contact lists, relay lists, and parameterized replaceable events are
whole-value replacements. Reading an empty or stale cache and publishing only
the field an app meant to change can erase fields that exist elsewhere.

The canonical failure is a contact-list button that appends one `p` tag to an
unestablished local value. The new kind:3 wins by timestamp and silently
deletes every contact, relay hint, petname, content string, and unrelated tag
the app failed to copy.

An app-side read/modify/write helper cannot make that safe. The protocol edit,
source selection, preservation rules, signing, routing, replay, and receipt
must be one NMP-owned operation.

## The implemented contract

### 1. Submit the operation, not a rewritten event

NIP-02's materializer is compiled into the engine's capability set at
construction, and the action submits a versioned operation that means follow or
unfollow one decoded public key. The action freezes the
selected author before custody. It does not wait for a relay-ready base, open
an acquisition worker, or expose source ids to the app.

### 2. Materialize over NMP's best source

The NIP-02 module owns kind:3 parsing and editing. It preserves the base
content and every unrelated tag byte-for-byte and in the same order.

- Follow appends one minimal valid `p` tag.
- Unfollow removes every matching `p` tag and nothing else.
- Repeated or opposing operations are folded in order into one complete value.

The app never reconstructs kind:3 and the UI never owns a second optimistic
follow Boolean.

When no source is known, NIP-02's capability-defined default is one complete
empty kind:3 for the frozen author. The requested operation is applied to that
value immediately. This grants no claim that Nostr is globally empty: a later
relay event remains eligible to become the source.

### 3. Keep one durable operation and receipt

After acceptance, the edit uses the normal durable write path: frozen author,
signer selection, canonical pending row, author-outbox routing, retry ownership,
and per-relay receipt facts. Dropping the button or action observer does not
cancel the durable obligation.

The retained operation, its source/program identity, and its ordinary receipt
survive restart. A newer valid relay event is materialized with the same
operation, producing a successor generation under the same receipt. The app
does not refetch, rebuild, retry, or allocate a second lifecycle.

The action's status stream is the ordinary receipt stream. Immediate typed
failure covers malformed target, signed-out account, engine closure, or an
unavailable receipt. A missing automatic-route provider is refused before
custody.

## Extending the pattern

`ReplaceableOperation` is the generic Rust write payload for protocol modules
that need retained, replayable semantic edits. Only an engine-issued
registration can mint it. The raw write API deliberately cannot: an
application reaches it through a typed protocol action that owns the schema
and policy.

Another replaceable protocol helper must still define and falsify:

- its read routing and source requirement;
- exact preservation rules for fields it does not own;
- first-value policy when the source-scoped base is `None`;
- operation ordering and successor-settlement policy;
- access-context isolation for private or AUTH-scoped state.

The existence of the generic guard does not bless arbitrary app-authored
read/modify/write helpers.

## Proof

The shipped falsifiers cover:

- tag order, content, relay hint, petname, duplicate-target, and unrelated-tag
  preservation;
- capability-default first value, cached-source materialization, restart, and
  later-source replay under one receipt;
- signed-out and providerless refusal without a write;
- a real loopback indexer/outbox relay through direct Rust: initial state,
  follow/ACK, reactive following state, duplicate
  follow, preservation of an existing contact, unfollow/ACK, and reactive
  not-following state.

---

<!-- nav-footer -->
<sub>← [Writing: accepted intent, local state, and relay evidence](14-writing.md) · [Index](README.md) · [Identity](16-identity.md) →</sub>
