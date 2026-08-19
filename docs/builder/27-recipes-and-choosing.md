# Using protocol modules

Core gives every app the raw two-noun engine. Enable an exact protocol module
when you want NIP-aware builders, parsing, reconstructed state, query fragments,
semantic operations, or typed context without hand-writing that protocol in app
code.

## Modules are optional semantic libraries

Enabling a module adds protocol knowledge around the same engine:

```swift
import NMP
import NMPNip29
import NMPNipC7
```

It does not add an NMP app container, register scene lifecycle, create another
store, or open its own relay pool.

The exact Cargo/SwiftPM/Gradle packaging is provisional. Opt-in code weight and
one canonical engine path are not.

## Closed reusable declarations

A helper may package the public binding grammar:

```swift
let authors = Nip02.myFollows()
let selection = NMPFilter(
    kinds: .literal(callerSelectedKinds),
    authors: authors
)
```

`myFollows()` expands to the NIP-02 contact-list projection. NMP can print,
hash, deduplicate, re-root, and diagnose it exactly as if the app wrote the
`Derived` graph inline.

NIP-02 owns the declaration. Core does not attach kind:1, a timeline, ranking,
or any other feed policy to it.

Apps and third-party packages may publish similar constructors over public
values. A helper is not a new reactive primitive or hidden subscription.

## Composing across exact owners

Some app features span two protocols. They compose across module boundaries;
they do not relabel one module's value inside another (#858):

```swift
for await snapshot in try engine.observe(currentAccountGroupListDemand()) {
    // NMP's NIP-29 product capability decodes the observational NIP-51 kind:10009 list.
    guard let list = snapshot.rows.first.map(parseSimpleGroupsListTolerant)
    else { continue }
    // The app selects one entry and names the relay(s) it discovers on with
    // NIP-29. A group can live on more than one relay, so the app names a SET
    // -- here a singleton, since the list only carried one host per entry.
    guard let selected = list.items.first else { continue }
    let scope = try NMPRelayScope.on([selected.hostRelay])
    let group = scope.group(selected.groupID)
    // Content selection is schema/app-owned; NIP-29 does not invent a fixed
    // group content-kind catalog.
}
```

One product capability, no second projection:

- `nmp-nip29` exposes NMP's typed view of the NIP-51 kind `10009` Simple groups
  list, including every decode evidence field (malformed item count, private
  content). NIP-29 owns its group metadata, membership, role, and moderation
  schemas.
- The app owns selection and
  its scope-narrowed operations. It accepts the exact fields an operation
  needs (a relay set named once, a group id); NMP does not derive routing
  authority from the tolerant decode.

The underlying kind `10009` demand is rooted at current pubkey and acquired
through user-list authority, never through the currently selected group's
relay scope. The selected group remains app state. NMP maintains no parallel
cache, second projection, or protocol-specific subscription lifecycle.

Saving that selected group is a typed semantic action, not a whole-event
rewrite: `engine.addGroupToList(groupId:hostRelay:name:)` in Swift and
`engine.addGroupToList(groupId, hostRelay, name)` in Kotlin return the ordinary
receipt. Separate remove-group and add/remove-relay-in-use methods own only
their exact valid public tags. They preserve unrelated order, malformed
evidence, and private content bytes; the host inside a `group` tag never
becomes a publication destination.

## Semantic operations

Protocol operations can own multi-event/state rules that should not leak into
app code:

```swift
let scope = try NMPRelayScope.on([selectedHost])  // named once, never per-call
let group = scope.group("research")

let receipt = try group.createGroup(engine: engine, authorPubkeyHex: myPubkeyHex)
try group.editMetadata(engine: engine, authorPubkeyHex: myPubkeyHex, name: "Research")
let adminReceipt = try group.addUser(
    engine: engine, authorPubkeyHex: myPubkeyHex, pubkeyHex: memberPubkeyHex, role: "admin"
)
```

NIP-29 owns the exact management events, tags, validation, group-state
transition, and relay-scope authority required by those operations — the app
never passes a host, a route, or an `h` value to any of them. The result
still uses core write receipts.

## Compose foreign drafts without stealing ownership

```swift
let message = NipC7.chat(text)
let receipt = try group.publish(message, using: engine)
```

- NIP-C7 owns the kind:9 chat event schema.
- NIP-29 adds only validated group context, including the `h` tag and the
  relay-scope authority the group's write routes to.
- Core freezes the final body, selects one signer, maintains one canonical row,
  and publishes one intent.

NIP-29 does not own kind:9 merely because a group can contain it.

## App policy remains app policy

The app still decides:

- which protocol queries exist;
- ranking, ordering, grouping, and presentation;
- product moderation policy and UX;
- labels, navigation, and account selection; and
- how typed module results fold into app state.

Protocol-defined moderation schemas, validation, and reconstructed moderation
state belong to the owning module. How the product applies and presents that
state belongs to the app.

## Choosing the owner

1. Universal store/sync/routing/signing machinery belongs in core.
2. A fact or state machine defined by one protocol belongs in that exact
   protocol module.
3. A closed constructor over public values may live in a module, app package,
   or third-party convenience package.
4. Behavior products can reasonably disagree about belongs in app code after
   delivery.

The fact that many apps want a convenience is evidence for packaging, not
permission to make its content model core.

---

<sub>[Index](README.md) · Related: [Protocol module authoring](32-extending.md) · [Source and routing context](17-relays.md) · [Kind-diverse examples](31-gallery.md)</sub>
