# Glossary

## Nostr terms

- **Event:** a signed NIP-01 value containing id, pubkey, created-at time, kind,
  tags, content, and signature.
- **Kind:** an integer identifying an event schema. Its meaning belongs to a
  NIP, another protocol specification, or the application that defines it.
- **Filter:** the NIP-01 selection value used in `REQ`, including ids, authors,
  kinds, tags, time bounds, limit, and optional extensions.
- **Relay:** a WebSocket server that accepts Nostr protocol messages and may
  store/serve events.
- **REQ / CLOSE:** NIP-01 wire messages that open and close relay
  subscriptions. NMP apps do not manage their ids directly.
- **EOSE:** one relay's statement that it finished sending stored events for
  one request. It is not proof of global completeness.
- **NIP:** a numbered Nostr Implementation Possibility defining protocol
  behavior or a convention.
- **Replaceable event:** an event whose newer winner supersedes an older event
  under kind/address rules rather than merely adding another row.
- **Addressable event:** a replaceable event keyed by kind, author, and `d` tag.
- **NIP-65 outbox model:** discovery of an author's declared write relays, used
  to acquire that author's events or route their publication.
- **Negentropy:** NIP-77 set reconciliation that identifies which event ids one
  side is missing without replaying the entire set.
- **AUTH:** NIP-42 relay challenge/response. The identity used can change what
  one source returns, so it participates in access context.
- **npub / nsec / hex:** public-key, secret-key, and raw encodings. Secret
  material never belongs in the event/outbox store.

## NMP terms

- **Live query:** the read workload observed through a native reactive stream.
- **Demand:** `Selection + SourceAuthority + AccessContext`, the semantic
  descriptor NMP keeps live.
- **Selection:** a Nostr filter whose set-valued fields may contain bindings.
- **Binding:** `Literal | Reactive(CurrentPubkey) | Derived | SetOp`, the closed
  grammar for a selection field's value.
- **Selector:** `Authors | Ids | Tag(char) | AddressCoord`, the closed
  projection vocabulary used by `Derived`.
- **Source authority:** a typed value saying which routing facts may acquire a
  selection. It is not a raw relay override.
- **Access context:** typed identity/visibility context that may change a
  source's answer.
- **Query snapshot:** current canonical local rows plus cache, acquisition, and
  shortfall evidence for one descriptor revision.
- **Acquisition evidence:** compact facts about currently planned sources, such
  as connecting, AUTH-blocked, EOSE-observed, reconciled, disconnected, or
  error.
- **Shortfall:** explicit intended work NMP could not perform because a source,
  route fact, access requirement, or local limit prevented it.
- **Watermark:** persisted reconciliation evidence for one source/filter window,
  never a global-completeness claim.
- **Write intent:** an immutable draft plus durability, typed context, and an
  optional signer override, observed through a receipt.
- **Pending row:** the canonical local store row created by durable acceptance
  before a signature is attached.
- **Receipt:** reattachable observed facts for a write. Durability controls
  whether the publication obligation resumes, not whether failure is visible.
- **Current pubkey:** one app-supplied reactive input and the default signer
  identity selection. It is not an account manager or cache partition.
- **Capability/provider:** a bounded signer, AUTH, encrypt, or decrypt operation
  NMP can invoke without receiving raw app closures for routing/demand.
- **Protocol module:** optional owner of exact protocol schemas, validation,
  reconstructed state, closed declarations, semantic operations, and typed
  context.
- **Lane:** an internal typed reason a source/relay participates in compiled
  work.
- **Diagnostics:** permanent read-only proof of graph, store, routing,
  transport, outbox, limit, and error state.
- **Re-root:** incrementally re-resolve only graph nodes depending on a changed
  reactive input.

---

<sub>[Index](README.md) · Related: [Mental model](02-mental-model.md) · [Current status](03-status-map.md)</sub>
