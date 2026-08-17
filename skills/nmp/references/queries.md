# Queries

## Model

A query descriptor is a value, not a callback:

```text
LiveQuery  = branches: [Demand] (1..=64) + aggregate_result_limit: Option<usize>
Demand     = selection Filter + ReadRouting + AccessContext + CacheMode + Freshness
Binding    = Literal | Reactive(ActivePubkey) | Derived | SetOp
Derived    = { inner: Demand, project: Selector }
Selector   = Authors | Ids | Tag(name) | AddressCoord
SetOp      = { op: SetAlgebra, operands: [Binding] }
SetAlgebra = Union | Intersect | Diff
Freshness  = Live | MaxAge { seconds } | CacheOnly
```

`LiveQuery` is the observation noun on every tier: `Engine::observe(query, window)` in Rust, `observe(NMPLiveQuery, window:)` in Swift, and the `NMPLiveQuery` overloads in Kotlin. A single filter or demand is the one-branch case, and the convenience overloads build it for you.

Branch order and duplicates are canonicalized, so any permutation of the same branch set is the same value. At most `MAX_BRANCHES` (64) branches are accepted. Construction refuses with `LiveQueryError`: `EmptyUnion`, `AggregateResultLimitZero`, `NestedAggregateResultLimit`, `TooManyQueryBranches { requested, maximum }`.

`aggregate_result_limit` caps the union across branches and is distinct from a per-branch `Filter.limit`; nesting an aggregate limit inside a branch is refused.

Filter fields are kinds, authors, ids, indexed single-character tags, since, until, and limit. `Selector::Tag(name)` projects already acquired rows locally and may use arbitrary tag names; that is different from the filter tag map's NIP-01 single-character keys.

`Derived.inner` is a full `Demand` on Rust, FFI, Swift, and Kotlin. The inner
query declares source, access, cache, and freshness independently from the
outer query; no platform implicitly inherits or reapplies defaults.

## Source and cache rules

- `Auto` requires an authors binding.
- `Pinned` requires a nonempty relay set and asks only those relays.
- `CacheMode::Strict` matters only with pinned authority; it limits cached rows to provenance intersecting the pinned relay set.
- `AccessContext` is `Public` or `Nip42(expectedPublicKey)`. NIP-42 freezes the
  expected identity in the demand; active-account changes cannot redirect it.
- `Freshness` is per-handle acquisition policy — `Live`, `MaxAge { seconds }`, or `CacheOnly` — and is deliberately excluded from atom, wire, and coverage identity, so it never splits a shared subscription.

## Delivered state

Rust `Subscription::recv` delivers a `Frame`: `deltas`, `window`, `evidence`, and `execution`. Which of the first two is populated is derived from boundedness, never a knob — an unbounded observation carries exact rebased `deltas` and `window: None`; a windowed observation carries `window: Some(WindowContents { rows, load })` and no deltas. Swift `NMPQuery` and Kotlin's Flow bridge collapse both arms into one `RowBatch`: they fold the deltas of an unbounded observation into accumulated state, and replace state wholesale from a windowed observation's authoritative rows. A `Row` contains the event and the set of relays that *hold* it — not whatever delivered it first — so the set grows as more relays are proven to carry the same event.

`Frame.evidence` / `RowBatch.evidence` is a list with one entry per canonical branch, in branch order. Read it by branch index; do not treat it as a single value. Each entry contains per-source `reconciledThrough` and current source status, plus `NoPlannedSource`, `NoResolvedDemand`, or `LocalLimit` shortfalls. `SourceStatus` is `Requesting`, `FinishedStoredEvents`, `AwaitingRequest`, `CoverageSatisfied`, `Connecting`, `Disconnected`, `AwaitingAuth { phase }`, `AuthDenied`, or `Error`; the AUTH states are populated for `Nip42` sessions, `AwaitingRequest` means the request is planned and locally owned but the transport has not accepted it yet, and `CoverageSatisfied` means a `MaxAge` scope was satisfied from durable coverage at open time and owns no send attempt. These are scoped facts. They do not prove the Nostr network is complete.

`Frame.execution` is direct-Rust only — it does not cross the FFI, so Swift and Kotlin have no equivalent. It is an ordered list of `ObservationEvidence` facts for this exact observation, each with a `sequence`, the canonical `branch` it came from (`None` for observation-wide facts), and a `kind` of `reactive_input`, `derived_set`, `concrete_filter`, `relay_request`, `request_settled`, `relay_closed`, `request_deferred`, `withdrawn`, or `overflow`. A bounded slow-consumer loss appears as an explicit `overflow` item rather than a silent gap. Windowed frames carry an empty `execution`.

### Ending a bounded read

To take a snapshot once its sources have answered, wait on `FinishedStoredEvents`, never on a timer. It means this relay reached NIP-01's end of stored events for this query's request — it sent everything it had. That is a fact, and it arrives on a frame; a quiet period is a guess that gets slower and wronger as relays do.

Two things it is not:

- **Not a claim that anything was proven.** `reconciledThrough` answers that, and the two disagree in both directions: a request you bounded with a `limit` finishes with no watermark at all, and a watermark from an earlier window is present while a fresh request is still streaming. Read `FinishedStoredEvents` for "nothing more is coming from here", `reconciledThrough` for "this relay proved this window".
- **Not a query verdict.** It is one source's fact. Rolling several into "the query is done" is your policy and yours alone — decide what a still-`Requesting` relay, a `Disconnected` one, and a shortfall each mean to your screen. NMP will not make that call, and no value on this surface ever will.

A relay that refuses the request, disconnects, or simply never answers has not finished, and no amount of waiting changes that. Bound your own read with a timeout if you need one — that is an app-owned deadline on how long you will wait, not a claim about the relay.

In an app accumulator over an *unbounded* observation — a windowed one replaces state wholesale from each authoritative frame, so there is nothing to fold:

- add or replace by event id when a full row arrives;
- update only sources on a `RowDelta::SourcesGrew` delta, in place — it is never an insertion;
- remove by id on retraction;
- apply app-owned sorting and windowing after accumulation.

## Pagination and observation ownership

Changing a filter means observing a new value. It is not mutation of an existing query. When extending a time window, overlap safely and deduplicate by event id; keep the earlier observation only as long as needed for the transition.

For backfill, prefer a windowed observation: pass a `Window::Expandable { initial, max }` to `observe` and grow it with `requestRows`, never above the window's `max`. `observe` refuses `WindowInitialExceedsMax`, `WindowSelectionHasLimit`, and `WindowAggregateResultLimit` rather than silently reinterpreting a conflicting request; Swift and Kotlin add `WindowZeroRows`, which direct Rust makes unrepresentable by taking `NonZeroUsize`.

Growth is declarative and idempotent — `requestRows(atLeast:)` states the total you want, so there is no continuation token to go stale. Its only thrown failures are `Unwindowed`, `EngineClosed`, and `StoreUnavailable`. The *outcome* arrives in band as the `WindowLoad` fact on the next frame (`Frame.window.load`, `RowBatch.load`): `Idle`, `Requesting`, `Returned { added }`, or `AtBound { max }`. `AtBound` is a fact, not an error, and `Returned { added: 0 }` means only that the planned advance added no canonical row — never that no older event exists. There is deliberately no Complete/End/Synced variant; read the per-source evidence for that judgment.

A Rust `Subscription` withdraws itself on `Drop`; `cancel()` is the same thing spelled as intent. Because `recv()` blocks, a drain thread usually owns the subscription outright — `cancel_handle()` hands another thread an `ObservationCancel`, and `window_handle()` hands it a `WindowHandle` that can `request_rows` or `cancel` the same windowed observation. Every one of those paths converges on a single withdrawal, however many callers fire.

Swift observations subscribe eagerly when constructed. Call `cancel()` or release the observation owner; teardown is iterator-owned. Kotlin's *unwindowed* overloads return a cold `Flow`, and each collector opens its own query, so share with `stateIn`/`shareIn` and a lifecycle-bound scope when one query should feed multiple consumers. Kotlin's *windowed* overloads instead return an `NMPQuery` whose `frames` flow claims collection exactly once — a second collector is refused rather than opening a second query. An ordinary or windowed query can fail with `NMPError.ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening; relay connection failure remains source evidence. Map/catch the setup error into feature state upstream of sharing so one failed subscription does not silently cancel the long-lived sharing scope.
