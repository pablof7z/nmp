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
    inner: NMPFilter(
        kinds: [3],
        authors: .reactive(.activePubkey)
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

## Pinned-host group timeline

Goal: read and write one NIP-29 group without widening its host authority.

Swift/Kotlin call shape:

```text
groupContentDemand(host, groupId)
engine.observe(demand)
engine.groupMessageIntent(host, groupId, content, recipients, reply)
engine.publishComposed(intent)
```

Rules:

- Treat `(host, groupId)` as the group identity. Do not union events with the same group id from another relay.
- The helper returns pinned authority. Do not replace it with a generic filter plus app relay list.
- Sort the accumulated rows in the app. Preserve each row's source proof and the query evidence.
- `groupMessageIntent` derives active author and time, protocol tags, reply/recipient rows, previous-state provenance, and pinned routing. Do not hand-build those fields in Swift/Kotlin.
- The composed intent is take-once. A new user decision requires a freshly composed intent.
- Keep the receipt id and observe all relay outcomes. One ACK is not universal delivery.

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
6. Distinguish attached, not found, and retained-but-unreadable. Reattachment does not reproduce every transient `Routed`/`Sent` fact; journal those live facts separately if the product needs a complete historical activity log.
7. Remove the app's receipt pointer only under explicit product retention policy after terminal evidence has been handled.

Receipt bridge admission now happens before core acceptance, so executor saturation or OS-thread refusal returns without an accepted obligation; composed publication also leaves its take-once intent unconsumed on this refusal path. One lost-id window remains because receipt enumeration does not exist: process loss after a successful return but before app persistence. State that limitation rather than claiming perfect app-level recovery, and do not blindly publish a replacement for an obligation whose id is unknown.

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

## NIP-46 signer handoff

Goal: connect a remote signer without treating OS launch as readiness.

1. Create an invitation and derive/cache its signer-specific URI or Android handoff while the invitation is still live. Invitation connection consumes it, so materializing the handoff afterward fails. Then begin `connectNip46`, start state observation, and only then launch the cached handoff.
2. On iOS, query only declared schemes. On Android, use the exact package from `androidHandoff` and launch explicitly to that package.
3. Observe connection states and wait for `ready`; a successful `open`/`startActivity` is only handoff evidence.
4. Activate `ready`'s user pubkey with `setActiveAccount` before an unsigned operation such as `groupMessageIntent`. Signer registration does not select the active account.
5. Handle synchronous outer `ExecutorSaturated` or `ThreadUnavailable` as connection admission refusal with no returned handle. Invitation capacity is reserved before take: saturation leaves the invitation reusable, but a later OS spawn failure consumes it and requires a fresh invitation/handoff. After a handle exists, inner session/relay refusal arrives as streamed `failed(reason)`/`Failed` followed by closure; do not relabel it as a timeout or reconstruct a typed error from the reason.
6. Keep the exact returned connection as the ownership token and close it deterministically. Closing an older replaced registration must not detach a newer one.
7. Never log invitation secrets, bunker credentials, or full handoff URIs.

Amber is catalogued as NIP-55-only, not a NIP-46 target. The current Kotlin artifact is desktop JVM plumbing, not a complete Android AAR or NIP-55 execution layer. It ships no Android build/test command, ABI/minSdk packaging contract, secure NIP-46 credential vault, or automatic signer reconnection. The connection exposes no reusable invitation credential; on restart the host must already own protected bunker/reconnect material or perform a fresh handoff, wait for `ready`, and reactivate the account.
