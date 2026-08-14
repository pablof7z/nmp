# Practical recipes

Use these as starting shapes, then verify exact declarations for the selected platform in the [Source map](source-map.md). They describe ownership and sequencing; they are not a substitute for app-specific product decisions.

## Account-aware home feed

Goal: one live feed whose author set follows the current account.

1. Construct one engine at the application/service boundary, with a persistent `storePath` when restart cache and receipts matter. On Apple platforms use durable Application Support storage, not a purgeable Caches location.
2. The reactive feed may open signed out and reroot when `engine.session.makeCurrent(account)` changes selection. Restore/select the account first only when the product should avoid a signed-out intermediate state.
3. Build a filter for the content kinds with authors bound through the current-account/follows graph supplied by the selected tier. If locally accepted posts should appear even when the user does not follow themselves, union the reactive current pubkey into the author binding. If the ergonomic facade does not project the required graph, stop and report the gap; do not query a contact list in the app and manually reopen a second author subscription.
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
3. Parse row content with `parseNostrContent` when you want source ranges and resolved locators; otherwise parse raw event content in app-owned presentation code. Parsing is pure and owns nothing.
4. Keep one current profile projection for display, but do not persist it as an authority beside NMP's canonical store.
5. If a nested reference must be live, open an ordinary query for its resolved locator and keep that query in the view-model or feature owner, with ordinary cancellation.
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

// Who is in these groups, and what are they called: the relay-signed records.
let watching = scope.observe(&engine, predicate, [Metadata, Admins, Members], None)?;
while let Some(snapshots) = watching.next().await? { /* GroupSnapshot per group */ }

// A directory: every room this relay advertises, 250 per host.
let browsing = scope.observe(&engine, nip29::all(), [Metadata], Some(250))?;

// One known room, no predicate and no id lookup:
let room = nip29::group(hosts, group_id)?.observe(&engine, [Metadata, Members])?;

// This group's CONTENT stays an ordinary live query through the one door.
engine.observe(group.read(contentFilter)?, None)?
```

`group.read` takes one ordinary app-supplied `Filter` and RETURNS one
`LiveQuery` -- `Single` for one host, `Union` of complete per-host branches for
more -- never a per-host list the app merges itself. Hand that result straight
to `engine.observe(query, None)`. The records observation folds the same
per-host branches for you and delivers a complete `GroupSnapshot`; you never
see a row delta.

`group.read` has two typed refusals, both about ownership:

- a selection naming 39000/39001/39002
  (`GroupContextError::RecordsAreNotContextScoped { kinds }`) -- those key on
  `d`, not `h`, so an `h`-scoped filter over them
  matches nothing forever and an app could not tell that apart from a group
  whose relay published no roster. Read them through the records observation
  instead.
- a selection that already constrains `#h`
  (`GroupContextError::CallerSuppliedContextConstraint`) -- the group id the
  scope retained is the sole semantic source of that row, so a caller's own is
  refused rather than silently overwritten. The same rule refuses a draft that
  arrives carrying `h` on the write side.

`GroupReadError::Declaration` is the separate refusal for a scope naming more
hosts than one observation supports (`LiveQuery::MAX_BRANCHES`) -- a refusal,
never a degraded answer.

Across hosts, the lists UNION (every entry carries the hosts that named it)
and the metadata does NOT -- one host's whole record wins on `created_at`,
never a field-wise merge. `snapshot.differs(record)` says whether the hosts
disagree; `snapshot.at(&host)` is exactly what one relay signed.

Branches scale with HOSTS, not groups: a hundred groups on two relays is two
branches. What a large watch list actually strains is the `#d` value set
inside one filter, which a relay may refuse or truncate -- shard across
several observations rather than assuming one will carry them all.

Rules:

- Treat `(host, groupId)` as the group IDENTITY *within a branch*: two relays
  hosting the same group id are two independent groups with the same name.
  Do not union events with the same group id from different relays; NMP keeps
  each relay's evidence separate on purpose.
- Every discovery/read branch carries pinned read authority to exactly its own
  host, stamped explicitly rather than inherited -- resolving evidence at one
  relay while listing at another would be a confidently wrong answer.
- Discovery IS the query language. `nip29::groups_whose_record_matches(Filter)`
  is the general spelling; `member_list_includes`/`admin_list_includes` are
  shorthands exactly equal to it, and `any_of(Binding)` takes any binding, so
  an app's own saved-groups lookup drives the observation reactively instead
  of being re-derived by hand. All build a `GroupIds`, composable with
  `union`/`intersect`/`minus`. Absence from a list is never proof of
  non-membership/non-admin.
- `nip29::all()` is "every group this host advertises" -- the ABSENCE of a
  `#d` row, not a `#d` row naming everything. Unbounded by nature; bound it
  with `observe`'s per-host `limit`. Advertisement is not enumeration.
  `all().minus(...)` does not typecheck: Nostr filters have no negation, so
  filter muted rooms out of the snapshots you render.
- `groups_whose_record_matches` refuses a kind outside 39000/39001/39002: it
  is evaluated with NIP-29's own pin, and a group host is authoritative for
  nothing else. Ids from the app's OWN data go through `any_of` as a derived
  binding carrying its own authority.
- Content kinds are selected by their real schema owners/app composition;
  NIP-29 has no fixed `[9,30315]` catalog.
- Sort the accumulated rows in the app. Preserve each row's source proof and the query evidence.
- Direct Rust holds a `Group` for the room's whole lifetime and publishes
  through it: `group.publish(&engine, author, builder)` appends the one `h`
  row before signing, routes `Explicit` to every host in the scope, and
  returns the ordinary `ReceiptStream`. It emits no `previous`, and a draft
  that arrives carrying `h` or `previous` is a typed refusal. The nine named
  operations all delegate to it: `add_users` (9000), `remove_users` (9001),
  `edit_metadata` (9002), `delete_event` (9005), `create_group` (9007, with an
  optional `parent` for subgroups — parenting is read off the create, not off
  `edit_metadata`), `delete_group` (9008), `create_invite` (9009),
  `join_request` (9021, publishable with no subscription at all, which is the
  case it exists for), and `leave_request` (9022). Each takes `(&engine,
  author: PublicKey, ...)`; `author` is an exact decoded key, never a reactive
  selector, because a semantic group write freezes who is writing at
  composition time.
- For a write that must carry several `h` rows, `scope.groups(ids)?.publish(...)`
  is the multi-context door; `group(id)` is the single-context one.
- The app never names a host on a write, never spells a routing value, and
  never touches `h` -- the relay set is named once, at the scope, not per
  operation. Do not hand-assemble a group write from raw tags plus a routing
  value.
- Every NIP-29 demand is `CacheMode::Strict`, not just pinned: a just-published
  message appears under a host once *that host* ACKs it, not immediately
  across the whole scope. Do not build UI that assumes simultaneous cross-host
  appearance -- per-host, on that host's own acceptance, is the correct and
  intentional behavior.

For rich rendering, parse row content with `parseNostrContent` and open ordinary queries for the locators you actually need live, bounded to a visible-plus-prefetch window keyed by stable event id. There is no content session, no claim, and no permit budget to manage — the budget is whatever query ownership the app imposes on itself. Swift's `NMPUI` builds `observeWhileVisible` components over the same idea, but `NMPUI` is not in the prepared-product catalog — it lives in this repository's qualification package, so an app writes that visibility scoping itself today.

## Follow button and relationship state

Goal: make a follow control reflect canonical contact-list state rather than optimistic local state.

Swift has `observeFollowing`, `follow`, `unfollow`, and the `NMPFollowing` resource. The action:

- acquires the existing contact-list base;
- preserves fields and tags it does not own;
- publishes a guarded replaceable edit; and
- streams acquisition, receipt, no-change, or typed failure facts.

Do not set `isFollowing = true` on tap. Render action progress separately until the live following snapshot changes. A missing reconciled contact list is an explicit refusal: ordinary follow must not create a first kind-3 list containing only one contact. Product onboarding must handle first-list creation as a distinct capability/workflow.

Both wrappers expose `observeFollowing`/`follow`/`unfollow` on `NMPEngine`; only the SwiftUI `NMPFollowing` observable object is Swift-specific. Do not import generated FFI types or reproduce contact-list editing in application code.

## Durable publishing and restart

Goal: accept a post offline, show honest delivery, and resume after process loss.

1. Restore the intended signer/account, then activate it.
2. Construct an unsigned `WriteIntent` with deliberate durability and routing.
3. Publish and persist `receipt.id` in app-owned durable state immediately.
4. Observe write facts independently from the query that renders the canonical row. The row is not an optimistic overlay created from the draft. Before `SigningState::Signed { event_id }` the public row exposes no intent/receipt id, so delivery UI must remain receipt-centric; correlate to a feed row only after the signed event id exists.
5. On restart, reopen the same NMP store, restore the same signer identity, then page `publishQueue(afterReceiptID:limit:)` to see what is outstanding and `reattachReceipt(id:)` / `reattachReceipt(correlation:)` to resume the ones you care about. `limit` is a required `UInt8` and `afterReceiptID` is the exclusive cursor, so enumeration is a loop, never one call. Writes parked on a missing signer end only by `cancel` followed by `removePublishQueueEntry`.
6. Distinguish attached, not found, and retained-but-unreadable. Reattachment traverses the durable `WriteFact` history in finite pages before streaming onward; stream lag is the typed `FactStreamLagged`, not silent loss.
7. Remove the app's receipt pointer only under explicit product retention policy after terminal evidence has been handled.

`Ok` from `publish` is acceptance, so a returned id names a write actually in custody. Process loss before you persist the id is recoverable three ways: mint a `correlation` token and persist it *before* publishing, then reattach by token; page the whole queue with `publishQueue(afterReceiptID:limit:)`; or, when a rendered row is what you have, join from its event id with `publishQueue(forEventID:afterReceiptID:limit:)` (Kotlin: `publishQueueForEvent`). That join is paged rather than single-valued on purpose — more than one receipt can own identical event bytes, and choosing one would hide the rest. Do not blindly publish a replacement for an obligation you have not looked for first.

## Relay-debug sheet

Goal: explain why one query is partial without inventing a health score.

Show three sections:

- Query evidence: planned sources, each source status and reconciled-through value, plus explicit shortfalls. This is `RowBatch.evidence` — one entry per canonical query branch, in branch order, never collapsed across branches.
- Engine diagnostics, per relay session: relay URL, the frozen access identity, exact wire filters, wire subscription count, authors served, lane counts, events by kind, per-filter coverage intervals, the NIP-11 and NIP-77 evidence fields, and — snapshot-wide — dropped merge rules, uncovered-author count, and transport degradation.
- Stuck writes: `DiagnosticsSnapshot.stalledWrites` is every durable obligation that cannot progress, answerable for an app holding no receipt at all, bounded to `stalledWriteTotals.detailLimit` rows in deterministic display order. Render the totals (`unroutable`/`unsignable`/`undeliverable`/`omittedDetails`) beside the detail rows: a bound on displayed rows must never read as a claim about how much is stuck. Reading it changes nothing.

Correlate by `(relay, access)`, not by relay URL alone — one URL hosts distinct sessions (`.public` versus a `.nip42` identity), each with its own diagnostics row, and merging them would credit one identity's coverage to another. Then compare the semantic demand with diagnostics' wire-filter JSON. There is no public stable query/filter identifier joining one evidence row to one exact diagnostic filter, and Swift's filter encoder is internal. Useful questions are: Was a source planned? Does the observed wire shape match the demand? Did events arrive? Is coverage present? Was a local cap reported?

Do not display `100% synced`, infer zero from missing coverage, or promise native fields for counters that stop short of your platform: `store_degraded` and `sessions_refused_by_subscription_budget` are reachable only from direct Rust, and `sessions_rejected_over_cap` stops at the raw UniFFI layer. `SourceStatus.awaitingAuth`/`authDenied`, `AuthPhase`, and `DiagnosticsSnapshot.authSessions` are live, populated states for `AccessContext::Nip42` demands, not reserved vocabulary awaiting a future implementation — a relay-debug sheet can render them today.

## Cache-first bounded list

Goal: render immediately while keeping work and UI bounded.

- Declare the semantic demand the feature needs; use a caller `limit` only when that is the actual selection semantics.
- Render cached rows on the first snapshot and update when evidence changes even if rows do not.
- Keep application sorting/windowing downstream from the full native snapshot.
- Do not keep overlapping pagination observations forever. When expanding a time window, overlap long enough to avoid a visual hole, dedupe by event id, then cancel the superseded observation.
- Treat `LocalLimit` or another shortfall as evidence that NMP could not cover the complete demand under current limits, not as an empty or complete result.
