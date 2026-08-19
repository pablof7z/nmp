# Using protocol modules

Core gives every app the raw two-noun engine. Enable an exact protocol module
when you want NIP-aware builders, parsing, reconstructed state, query fragments,
semantic operations, or typed context without hand-writing that protocol in app
code.

## Modules are optional semantic libraries

Enabling a module adds protocol knowledge around the same engine, for example
by depending on `nmp-nip29` or `nmp-nipc7` alongside the core `nmp` crate.

It does not add an NMP app container, register scene lifecycle, create another
store, or open its own relay pool.

The exact Cargo packaging is provisional. Opt-in code weight and
one canonical engine path are not.

## Closed reusable declarations

A helper may package the public binding grammar: for example, a reusable
`myFollows()` constructor that a caller composes into its own selection
alongside caller-chosen kinds.

`myFollows()` expands to the NIP-02 contact-list projection. NMP can print,
hash, deduplicate, re-root, and diagnose it exactly as if the app wrote the
`Derived` graph inline.

NIP-02 owns the declaration. Core does not attach kind:1, a timeline, ranking,
or any other feed policy to it.

Apps and third-party packages may publish similar constructors over public
values. A helper is not a new reactive primitive or hidden subscription.

## Composing across exact owners

Some app features span two protocols. They compose across module boundaries;
they do not relabel one module's value inside another (#858). Observing the
current account's group-list demand delivers a row that NMP's NIP-29 product
capability decodes as the observational NIP-51 kind:10009 list. The app
selects one entry and names the relay(s) it discovers on with NIP-29 — a
group can live on more than one relay, so the app names a set, a singleton
when the list carries only one host per entry. Content selection within the
resulting group scope is schema/app-owned; NIP-29 does not invent a fixed
group content-kind catalog.

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
rewrite: `add_group_to_list` takes the engine and the selected group entry and
returns the ordinary receipt. Separate remove-group and add/remove-relay-in-use functions own only
their exact valid public tags. They preserve unrelated order, malformed
evidence, and private content bytes; the host inside a `group` tag never
becomes a publication destination.

## Semantic operations

Protocol operations can own multi-event/state rules that should not leak into
app code: a relay scope named once (never per-call), a group selected within
it, and operations such as creating the group, editing its metadata, or
adding a user with a role, each returning the ordinary receipt.

NIP-29 owns the exact management events, tags, validation, group-state
transition, and relay-scope authority required by those operations — the app
never passes a host, a route, or an `h` value to any of them. The result
still uses core write receipts.

## Compose foreign drafts without stealing ownership

A NIP-C7 chat draft published through a NIP-29 group composes both owners into
one write: NIP-C7 builds the message, the group publishes it through the
engine.

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
