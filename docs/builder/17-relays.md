# Read routing and protocol routing context

Apps say where reads come from only when they want to override NMP. Otherwise
they say nothing and NMP works it out.

## Queries route themselves

```swift
let demand = NMPDemand(selection: selection)
```

That is the whole declaration. `routing` defaults to `.auto`, which authorizes
NMP to discover and use the NIP-65 write relays of every author the selection
resolves, and to follow relay hints and prior provenance. **The app relays the
operator configured are read from as well, always** — they are not a fallback
NMP reaches for when the other lanes come up thin. The app does not watch
kind:10002, build author-to-relay maps, group authors by relay, or reopen
requests as those maps change.

The lanes add up; the app never picks between them. See
`docs/internals/routing/outbox.md` for the ruling and its exact wording.

`.auto` is total: it has no precondition a selection can fail. An authorless
selection — "kind:1, wherever you find it" — is the same path with no authors
to solve for, not a different one, so there is no second routing word to learn
and no routing error to handle.

## Overriding it

The one override is an exact relay set:

```swift
let demand = NMPDemand(
    selection: selection,
    routing: .explicit([hostRelay])
)
```

`.explicit` asks those relays and nothing else — never widened to outbox,
directory, app, fallback or indexer relays, whatever NMP later learns. It must
be nonempty; an empty set is refused at the door rather than accepted and
silently unroutable.

These two words are the entire app-facing routing vocabulary, and they are the
same two `NMPWriteRouting` uses. There is no third one, and no generic
`relays: [URL]` escape hatch anywhere else on the surface.

## Access context is separate

The same relays may answer differently under different AUTH identities or
visibility grants, so access is its own axis:

```swift
let demand = NMPDemand(
    selection: groupSelection,
    routing: .explicit([groupHost]),
    access: .nip42(publicKey: groupIdentity)
)
```

A protocol module composes these for you rather than handing you a relay to
pass around: `NMPGroup.read` returns a live query already carrying one branch
per host, each `.explicit` to that host alone.

Selection, read routing, and access context all participate in descriptor
identity, safe wire sharing, diagnostics, and acquisition evidence.

## Writes carry typed routing context

Ordinary author publication publishes per the author's outbox, to the
operator's app relays, and — for kind:0, kind:3 and kind:1xxxx — to the
configured indexers. Those lanes are additive and all of them apply; the app
declares none of them:

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
