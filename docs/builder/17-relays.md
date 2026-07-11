# Source authority and protocol routing context

Apps declare semantic authority. NMP expands it into concrete relay work.

## Queries name source authority

```swift
let demand = NMPDemand(
    selection: selection,
    source: .authorOutboxes,
    access: .public
)
```

`authorOutboxes` authorizes NMP to discover and use authors' NIP-65 write
relays. The app does not watch kind:10002, build author-to-relay maps, group
authors by relay, or reopen requests as those maps change.

Other typed authorities may include:

- a protocol host relay that is part of the semantic object;
- recipient inboxes defined by a private-message protocol;
- operator-configured indexers for discovery; or
- a narrow relay set already validated by a protocol operation.

These are illustrative categories, not a generic `relays: [URL]` escape hatch.

## Access context is separate

The same source may answer differently under different AUTH identities or
visibility grants:

```swift
let demand = NMPDemand(
    selection: groupSelection,
    source: group.sourceAuthority,
    access: .auth(groupIdentity)
)
```

`group.sourceAuthority` is minted by the NIP-29 module after validating the
group reference and host. A plain relay URL cannot be promoted into protocol
authority by app code.

Selection, source authority, and access context all participate in descriptor
identity, safe wire sharing, diagnostics, and acquisition evidence.

## Writes carry typed routing context

Ordinary author publication uses engine-owned outbox discovery:

```swift
let receipt = try engine.publish(.init(
    draft: draft,
    durability: .durable
))
```

The app does not pass the author's current relay list. If routing facts change,
the durable intent may gain a new append-only relay lane without erasing prior
attempt evidence.

Some protocols make a relay part of the operation itself. That context comes
from the protocol module:

```swift
let group = Nip29.group(id: groupId, host: groupRelay)
let receipt = try group.publish(photoDraft, using: engine)
```

The public host is a semantic NIP-29 parameter. The module turns that pair into
opaque context usable only for that group operation; it does not grant a generic
relay override. NIP-29 contributes the group `h` tag plus host constraint. It
does not own the photo kind, select the signer, open its own relay connection, or
publish outside the core outbox.

## Routing reasons are typed and inspectable

The internal compiler may produce lanes such as:

```text
AuthorOutbox(author, relay)
ProtocolHost(protocolObject, relay)
RecipientInbox(recipient, relay)
IndexerBootstrap(operatorPolicy, relay)
```

Exact names are internal/provisional. The invariant is that every connection,
request, and publish lane has a reason traceable to a demand or accepted intent.
No relay is contacted "just in case" without a represented policy.

## Coalescing and caps preserve meaning

Compatible demand may share connections and widened wire filters when local
re-filtering preserves exact selection. NMP keeps descriptor attribution even
when wire work is shared.

A fan-out cap may bound work. It must then report uncovered source/author
shortfall. It cannot silently contact the first N relays and label the query
complete.

## Private routes fail closed

Private or recipient-specific protocols use narrow-only route types. If the
required inbox cannot be resolved, the write fails before any public fallback
relay is added. There is no generic union operation that can accidentally widen
a private route.

Encryption and relay routing remain distinct stages. A cryptographically valid
event is not automatically safe to publish to an arbitrary relay.

## What diagnostics must show

For each current lane, diagnostics retains:

- the descriptor/intent that required it;
- the typed source/routing reason;
- authors or protocol objects served;
- access/AUTH context reference;
- exact wire filter or signed event id;
- connection, EOSE, watermark, attempt, and error facts; and
- any cap or shortfall that prevented another lane.

That is how an app verifies self-routing without taking routing ownership back.

---

<sub>[Index](README.md) · Related: [Identity and signers](16-identity.md) · [Protocol modules](27-recipes-and-choosing.md) · [Tracing demand](18-tracing-demand.md)</sub>
