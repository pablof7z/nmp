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
import NMPNip68
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
for await snapshot in try engine.observe(activeAccountDemand()) {
    // NIP-51 decodes its own kind:10009 list, as itself.
    guard let list = snapshot.rows.first.map(parseSimpleGroupsListTolerant)
    else { continue }
    // The app selects one entry and discovers groups on its host with NIP-29.
    guard let selected = list.items.first else { continue }
    let discovery = try groupDiscoveryDemand(host: selected.hostRelay)
    // Content selection is schema/app-owned; NIP-29 does not invent a fixed
    // group content-kind catalog.
}
```

Two exact owners, no wrapper between them:

- NIP-51 owns kind `10009` Simple groups, including its public/private list
  codec, replacement construction, and typed list entries — plus every
  evidence field on the decode (malformed item count, private content).
- NIP-29 owns its group metadata, membership, role, and moderation schemas and
  its host-scoped operations. It accepts the exact fields an operation needs (a
  host, a group id) and claims neither kind `10009` nor kind `30002`. It does
  not depend on the NIP-51 package at all.

The underlying kind `10009` demand is rooted at current pubkey and acquired
through user-list authority, never through the currently selected group host.
The selected group remains app state. Neither module maintains a parallel
cache, a second projection of the other's value, or its own subscription
lifecycle.

## Semantic operations

Protocol operations can own multi-event/state rules that should not leak into
app code:

```swift
let group = try await nip29.createGroup(
    name: "Research",
    host: selectedHost,
    using: engine
)

let receipt = try group.makeAdmin(pubkey, using: engine)
```

NIP-29 owns the exact management events, tags, validation, group-state
transition, and host authority required by those operations. The result still
uses core write receipts.

## Compose foreign drafts without stealing ownership

```swift
let asset = try await blossom.upload(file)
let photo = try Nip68.buildPhoto(asset)
let receipt = try group.publish(photo, using: engine)
```

- Blossom owns upload and asset verification.
- NIP-68 owns the photo event schema.
- NIP-29 adds only validated group context, including the `h` tag and host
  authority.
- Core freezes the final body, selects one signer, maintains one canonical row,
  and publishes one intent.

NIP-29 does not own the photo kind merely because a group can contain it.

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
