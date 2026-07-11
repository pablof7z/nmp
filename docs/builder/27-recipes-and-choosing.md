# Protocol modules, reusable declarations, and app policy

**Status: TARGET.** The closed filter grammar is built. Opt-in protocol-module
packaging and semantic module operations are not yet shipped. This chapter
records the ownership boundary; examples are directional, not frozen API.

## No content kind is privileged

NMP core is a generic sync-and-routing engine. It does not ship a favored
`textNote`, `homeFeed`, `myFollowsNotes`, or other content-kind battery. Kind:1
is one event kind among many, not the center of the extension model.

Reusable conveniences are still welcome, but they belong to one of three
categories with different ownership.

## 1. Reusable closed declarations

A helper may package an existing, closed descriptor without hiding its
expansion. For example, a NIP-02 module can provide the binding commonly called
`myFollows`:

```text
Derived(
  inner: Filter(kinds:[3], authors: Reactive(ActivePubkey)),
  project: Tag(p)
)
```

This is not a new reactive primitive and not an app closure. NMP still sees the
entire value, so it can hash, deduplicate, route, re-root, and explain it.

Apps and third-party packages may define equivalent helpers. The core does not
need to bless one feed assembled from that binding.

## 2. Protocol-owned semantic functionality

An opt-in NIP module owns the protocol facts defined by that NIP:

- its event schemas and owned event kinds;
- codecs and validation;
- state reconstruction rules;
- typed references and semantic operations;
- reusable query declarations;
- protocol routing context.

For example, a NIP-29 module may expose group creation, administration, member
state, and a typed group reference containing its host relay. It owns NIP-29's
management/state events. It does **not** own every content kind that can be
published inside a group.

The module may contextually publish a foreign draft:

```text
photo = Nip68.buildPhoto(asset)
receipt = group.publish(photo)
```

`Nip68` owns the photo draft. The bound NIP-29 group adds only the NIP-29
context it owns, such as the correct `h` tag and host-relay routing. Core then
selects a signer once, signs once, stores once, and publishes once.

## 3. App policy

The app owns choices that are not protocol facts:

- what appears in a feed;
- ranking, ordering, and nesting;
- moderation and presentation policy;
- which queries exist and how long they live;
- how protocol results fold into app state.

A reusable helper can live in app code without becoming an NMP promise. "Many
apps need it" is evidence for convenience, not permission to specialize core.

## Composition rules

Protocol composition uses immutable unsigned drafts:

1. A schema module constructs a draft containing only the fields it owns.
2. A contextual module returns a new draft containing only its contribution.
3. The core freezes the final body at acceptance, selects the default or
   overridden signer, signs once, and publishes through the declared protocol
   route.

No module may mutate an already-signed event. No module may register opaque
closures into demand, routing, ordering, or admission. Contextual routing must
remain typed and introspectable.

## Relay rule

The app does not hand NMP an expanded relay list for ordinary queries or
author-outbox writes. That would move routing correctness back into app code.

A protocol module may carry a relay because the protocol makes that relay part
of the semantic object: a NIP-29 group host, an inbox set, or another typed
source authority. That is protocol context, not a generic relay override.

## Choosing the owner

Use this decision order:

1. **Is it generic store/sync/routing/signing machinery?** It belongs in core.
2. **Is it a fact or state machine defined by a NIP/protocol?** It belongs in
   the opt-in protocol module that owns that specification.
3. **Is it a closed declaration over existing public values?** It may be a
   reusable helper in that module or in a third-party package.
4. **Would different products reasonably choose different behavior?** It
   belongs in the app after delivery.

The primitive path remains public and explainable, but a semantic module
operation need not pretend to be only a one-line filter recipe. Some protocols
correctly own multi-event state, validation, and contextual routing.

---

<!-- nav-footer -->
<sub>← [Troubleshooting & FAQ](26-troubleshooting.md) · [Index](README.md) · [Patterns & anti-patterns](28-patterns.md) →</sub>
