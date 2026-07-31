# Practical recipes

Use these as starting shapes, then verify exact declarations for the selected platform in the [Source map](source-map.md). They describe ownership and sequencing; they are not a substitute for app-specific product decisions.

## Account-aware home feed

Goal: one live feed whose author set follows the active account.

1. Construct one engine at the application/service boundary, with a persistent `storePath` when restart cache and receipts matter. On Apple platforms use durable Application Support storage, not a purgeable Caches location.
2. The reactive feed may open signed out and reroot when `setActiveAccount` changes. Restore/select the account first only when the product should avoid a signed-out intermediate state.
3. Build a filter for the content kinds with authors bound through the active-account/follows graph supplied by the selected tier. If locally accepted posts should appear even when the user does not follow themselves, union the active pubkey into the author binding. If the ergonomic facade does not project the required graph, stop and report the gap; do not query a contact list in the app and manually reopen a second author subscription.
4. Observe once at the feature-model boundary. Swift owns one eager `NMPQuery`; Kotlin collects one cold flow and shares it with `stateIn` or `shareIn` if several consumers need it.
5. Replace the model's canonical input with every delivered native `RowBatch`. Apply ranking, mute policy, deduped UI sections, and pagination windows downstream.
6. Render cache rows immediately. Describe evidence per planned source: connecting, reconciled-through, disconnected, shortfall. Never convert it to global `synced`.
7. Cancel/release the query when the feature no longer exists. NMP withdraws demand and reconnects still-live demand itself.

Before promising this recipe cross-platform, check the exact follows binding/helper. A Swift-only following action does not imply a Kotlin following API.

The current Swift graph for followed authors is a derived contact-list query, projected through its `p` tags:

```swift
let followed = NMPBinding.derived(
    inner: NMPDemand(
        selection: NMPFilter(
            kinds: [3],
            authors: .reactive(.activePubkey)
        ),
        source: .authorOutboxes
    ),
    project: .tag("p")
)
let homeAuthors = NMPBinding.setOp(
    .union,
    [.reactive(.activePubkey), followed]
)
```

Use `homeAuthors` in the content filter with `.authorOutboxes`. The self-union is product semantics, not an NMP default.

## Profile screen with live content

Goal: show identity metadata and authored content without creating an app cache or hidden join.

1. Decode the route input with the platform's public Nostr-entity decoder when it may be `npub`, `nprofile`, or a `nostr:` URI. Reject unsupported entity shapes explicitly.
2. Open a replaceable metadata query and the content query as separate live demands. Each owns its source evidence and cancellation.
3. Let the profile/content resource layer parse row content if using `NMPContent`, and claim only nested reference occurrences/targets that must remain live; otherwise parse raw event content in app-owned presentation code.
4. Keep one current profile projection for display, but do not persist it as an authority beside NMP's canonical store.
5. If opening nested references, keep each claim/session in the view-model or feature owner. Cancel Swift claims before `stop()` on their session; close Kotlin claims before closing their session.
6. Test profile replacement and removal as live snapshot changes, not one-shot fetch completion.

This deliberately avoids a magic `loadProfileAndPosts` noun. NMP exposes composable live queries; the app owns the screen composition.

## NIP-29 group discovery and schema-owned timelines

Goal: discover groups on one or more relays without letting NIP-29 invent a
content catalog it does not own.

A group can live on more than one relay, so the app names its relay set once
and narrows it to a group:

```text
let scope = nip29::on(hosts)?          // RelayScopeError::EmptyRelaySet if empty
let group = scope.group(groupId)       // same hosts, narrowed to one group

engine.observe(scope.groups_where(&predicate)?, None)?   // discovery
engine.observe(group.read(contentFilter)?, None)?         // this group's content
```

Both `observe` calls take one ordinary `LiveQuery` -- `Single` for one host,
`Union` of complete per-host branches for more -- never a per-host list the
app merges itself.

Rules:

- Treat `(host, groupId)` as the group IDENTITY *within a branch*: two relays
  hosting the same group id are two independent groups with the same name.
  Do not union events with the same group id from different relays; NMP keeps
  each relay's evidence separate on purpose.
- Every discovery/read branch carries pinned read authority to exactly its own
  host, stamped explicitly rather than inherited -- resolving evidence at one
  relay while listing at another would be a confidently wrong answer.
- Discovery is evidence-scoped: `nip29::member_list_includes`/
  `admin_list_includes` build a composable `GroupPredicate`
  (`union`/`intersect`/`minus`) over observed kind:39002/39001 rows. Absence
  from a list is never proof of non-membership/non-admin.
- Content kinds are selected by their real schema owners/app composition;
  NIP-29 has no fixed `[9,30315]` catalog.
- Sort the accumulated rows in the app. Preserve each row's source proof and the query evidence.
- Direct Rust holds a `Group` for the room's whole lifetime and publishes
  through it: `group.publish(&engine, author, builder)` appends the one `h`
  row before signing and routes `Explicit` to every host in the scope. It
  emits no `previous`, and a draft that arrives carrying `h` or `previous` is
  a typed refusal.
- The app never names a host on a write, never spells a routing value, and
  never touches `h` -- the relay set is named once, at the scope, not per
  operation. Do not hand-assemble a group write from raw tags plus a routing
  value.
- Every NIP-29 demand is `CacheMode::Strict`, not just pinned: a just-published
  message appears under a host once *that host* ACKs it, not immediately
  across the whole scope. Do not build UI that assumes simultaneous cross-host
  appearance -- per-host, on that host's own acceptance, is the correct and
  intentional behavior.

For rich rendering, use Swift `NMPContent` resources or Kotlin `NMPContentClient(engine).session(...) -> NostrContentSession` for only a bounded visible-plus-prefetch window keyed by stable event id. Session policy limits are per session, not engine-global. Enforce a separate aggregate app permit pool before claiming a distinct target: use the reference-demand plan's `1 + helpers.count` as that target's query cost (one canonical query plus its helper queries), and cap the number of open row sessions independently. `claim(referenceID:)` in Swift / `claim(referenceId)` in Kotlin accepts an occurrence id from that session's parsed document and may return `nil`/`null`; it is not a row id or target key. Record the permits with the claim, then cancel/close claims and release their permits before stopping/closing the row's session on eviction.

## Follow button and relationship state

Goal: make a follow control reflect canonical contact-list state rather than optimistic local state.

Swift has `observeFollowing`, `follow`, `unfollow`, and the `NMPFollowing` resource. The action:

- acquires the existing contact-list base;
- preserves fields and tags it does not own;
- publishes a guarded replaceable edit; and
- streams acquisition, receipt, no-change, or typed failure facts.

Do not set `isFollowing = true` on tap. Render action progress separately until the live following snapshot changes. A missing reconciled contact list is an explicit refusal: ordinary follow must not create a first kind-3 list containing only one contact. Product onboarding must handle first-list creation as a distinct capability/workflow.

Kotlin currently lacks the ergonomic following resource/action. Report that limitation; do not import generated FFI types or reproduce contact-list editing in application code.

## Durable publishing and restart

Goal: accept a post offline, show honest delivery, and resume after process loss.

1. Restore the intended signer/account, then activate it.
2. Construct an unsigned `WriteIntent` with deliberate durability and routing.
3. Publish and persist `receipt.id` in app-owned durable state immediately.
4. Observe receipt facts independently from the query that renders the canonical row. The row is not an optimistic overlay created from the draft. Before `Signed(eventId)` the public row exposes no intent/receipt id, so delivery UI must remain receipt-centric; correlate to a feed row only after the signed event id exists.
5. On restart, reopen the same NMP store, restore the same signer identity, and call `reattachReceipt(id:)` / `reattachReceipt(id)`.
6. Distinguish attached, not found, and retained-but-unreadable. Reattachment reconstructs persisted relay/AUTH waits, retry eligibility, ambiguous handoffs, and `Sent` only where an exact durable lane has persisted `Written`; it does not reproduce transient `Routed` history or invent ephemeral handoffs. Journal non-retained live progress separately if the product needs a complete historical activity log.
7. Remove the app's receipt pointer only under explicit product retention policy after terminal evidence has been handled.

The receipt bridge starts as async work on the shared engine runtime before core acceptance, and there is no capacity or thread refusal on this path, so a returned id reflects an accepted obligation and a consumed composed intent. One lost-id window remains because receipt enumeration does not exist: process loss after a successful return but before app persistence. State that limitation rather than claiming perfect app-level recovery, and do not blindly publish a replacement for an obligation whose id is unknown.

## Relay-debug sheet

Goal: explain why one query is partial without inventing a health score.

Show two sections:

- Query evidence: planned sources, each source status and reconciled-through value, plus explicit shortfalls.
- Engine diagnostics: relay URL, exact wire filters, wire subscription count, authors served, lane counts, events by kind, coverage intervals, dropped merge rules, uncovered-author count, and transport degradation.

Correlate by relay where the two public projections provide one, then compare the semantic demand with diagnostics' wire-filter JSON. There is no public stable query/filter identifier joining one `SourceEvidence` row to one exact diagnostic filter, and Swift's filter encoder is internal. Useful questions are: Was a source planned? Does the observed wire shape match the demand? Did events arrive? Is coverage present? Was a local cap reported?

Do not display `100% synced`, infer zero from missing coverage, or promise native fields for Rust-only store degradation/rejection counters. Reserved AUTH vocabulary is not proof that the engine currently populates an AUTH lifecycle.

## Cache-first bounded list

Goal: render immediately while keeping work and UI bounded.

- Declare the semantic demand the feature needs; use a caller `limit` only when that is the actual selection semantics.
- Render cached rows on the first snapshot and update when evidence changes even if rows do not.
- Keep application sorting/windowing downstream from the full native snapshot.
- Do not keep overlapping pagination observations forever. When expanding a time window, overlap long enough to avoid a visual hole, dedupe by event id, then cancel the superseded observation.
- Treat `LocalLimit` or another shortfall as evidence that NMP could not cover the complete demand under current limits, not as an empty or complete result.

