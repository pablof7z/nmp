# refactor: an app says nothing and gets NMP's routing (`ReadRouting::Auto`) (#847)

## The defect, and the overcorrection

`Demand::from_filter` decided a query's wire routing by looking at whether its
`Filter` happened to bind `authors`:

```rust
pub fn from_filter(selection: Filter) -> Self {
    let source = if selection.authors.is_some() {
        SourceAuthority::AuthorOutboxes
    } else {
        SourceAuthority::Public
    };
    ...
}
```

Selection syntax is not routing. Deleting that inference is right, and the
first pass of this branch did it — by forcing all 199 call sites to name a
`SourceAuthority` explicitly.

**That was the wrong half of the fix.** Making every app name a routing value
to perform an ordinary read is not the cure for a bad default; it is the
absence of a default. An app should say nothing and get NMP's routing.

So this branch now does both: the inference is gone, *and* nothing has to be
said.

```rust
// Before this branch: a guess nobody could see.
LiveQuery::from_filter(filter)

// First pass: no guess, but everyone pays.
Demand::author_outboxes(filter).expect("the selection binds `authors`")

// Now: no guess, and nobody pays.
Demand { selection: filter, ..Demand::default() }
```

## The surface

```rust
pub enum ReadRouting {
    /// "Figure out where to read this from." NMP applies whatever routing
    /// rule fits the demand: author outboxes (NIP-65 outbound) for a
    /// selection that resolves authors, a group's host relays, a DM inbox,
    /// relay hints and prior provenance, then the operator's app and
    /// fallback lanes. Outbox is the typical case, not the definition.
    #[default]
    Auto,
    /// "Ask these relays and that is that." Never widened to outbox,
    /// directory, app, fallback or indexer relays.
    Explicit(Vec<nostr::RelayUrl>),
}
```

The names are not new. `docs/internals/routing/auto-and-explicit.md` records
the ruling that "the whole app-facing routing vocabulary is these two words",
already live as `WriteRouting { Auto, Explicit(Vec<RelayUrl>) }`. Reads now
speak the same two words as writes, spelled identically.

`Demand.source` becomes `Demand.routing`. `SourceAuthority` is deleted — no
alias, no re-export — along with every `Ffi*`/Swift/Kotlin mirror.

### This is a reduction, not a rename

The clearest evidence that three values were never three peers is the old
`Public` variant's own doc comment, which described it as

> Routed via operator-configured lanes (indexer/app/fallback) or protocol-fact
> pinned lookups (NIP-29 group host, DM inbox kind:10050)

That *is* "whatever routing NMP determines" — it was already the default,
wearing a name that made it look like a sibling of `AuthorOutboxes`. One was a
mechanism; the other was the fallback for everything that was not that
mechanism. They were never peers, and `Auto` is what `Public` was actually
describing all along.

So `AuthorOutboxes` and `Public` are **deleted, not renamed**, and collapse
into `Auto`. `Pinned(BTreeSet<RelayUrl>)` becomes
`Explicit(Vec<RelayUrl>)` — byte-identical in shape to `WriteRouting::Explicit`.

`DemandError` loses `AuthorOutboxesRequiresBoundAuthors` outright: `Auto` is
total, so there is no selection shape it can fail against, and therefore no
routing error an app must handle. The nonempty refusal survives as
`ExplicitRequiresNonemptyRelaySet`. At the FFI boundary that is the only
`Demand` refusal that still crosses.

### What this removes that an app could previously express

Stated plainly rather than buried: you can no longer say **"these authors, but
do not chase their outboxes."** `Public` over an author-bearing selection was
constructible from Rust, FFI, Swift and Kotlin, and meant exactly that.

That is deliberate. The settled contract is that an app says "figure it out" or
"these exact relays" — and "these authors but only via operator lanes" is
precisely the third thing being deleted. An app that genuinely wants to
constrain where reads come from names `Explicit(relays)`, which is stronger and
says what it means.

## Atom identity: which collapses become possible

`docs/internals/conventions/` and bug-class ledger #18 require that two queries
with the same `Filter` but different intended routing never share an atom,
refcount, coverage or attribution identity. This change collapses two of three
routing values, so the honest accounting:

**One collapse is now possible that was not before.** A demand that would have
been `AuthorOutboxes` and one that would have been `Public`, over the same
selection and access, are now **one** atom: one `ContextualAtom` hash, one
`CoverageKey`, one `DemandKey`, one wire subscription. Previously two of each.

**Nothing else collapses.** The source axis still participates in identity; it
narrowed from three values to two:

| Pair | Before | After |
|---|---|---|
| `AuthorOutboxes` vs `Public` | distinct | **collapsed** |
| `Auto` vs `Explicit(R)` | distinct | distinct |
| `Explicit(R1)` vs `Explicit(R2)` | distinct | distinct |
| any routing under different `AccessContext` | distinct | distinct |

Ledger #18 survives because the collapse removes a *choice the app no longer
makes*, not the axis. Proven by
`auto_and_explicit_over_one_selection_are_distinct_identities` (grammar),
`contextual_atom_hash_distinguishes_identical_filters_under_different_read_routing`
(grammar), `coverage_key_differs_for_different_read_routing` (store),
`for_wire_distinguishes_identical_filters_under_different_read_routing` and
`for_wire_distinguishes_explicit_routings_with_different_relay_sets` (router),
and `get_coverage_distinguishes_auto_from_an_explicit_relay_set` (engine).

### Refcounting

`active_outbox_authors` is incremented at two sites, decremented at two, and
rebuilt at a fifth. Each previously asked "is this atom `AuthorOutboxes`?" —
five copies of one predicate, which is the shape that lets counting and
decrementing drift apart and leaves a demand whose request never closes.

All five now call one function, `route::outbox_authors(&atom.filter,
&atom.routing)`, so they cannot disagree by construction. Pinned by
`the_outbox_author_refcount_returns_to_zero_across_auto_and_explicit`.

## `Auto` is one total path

`route::classify` no longer branches on filter shape at all:

```rust
pub(crate) enum AtomClass { Auto, Exact(BTreeSet<RelayUrl>) }
```

The `Coverage`/`Supplemental` split is gone. Every `Auto` atom now runs the
same path: the coverage solve over whatever authors it resolves, its projected
hint/provenance facts routed **directly**, then the operator app and fallback
lanes. A selection that resolves no authors is the degenerate case — empty
candidates, empty solve, operator lanes carrying the whole route — not a second
class.

Two consequences worth naming:

- **The direct hint lane stays narrow.** `provenance_for_projected` runs only
  when the group is unbound. That is the case that motivated it: an unbound
  selection resolves no authors, so its hints have no author to enter the solve
  as candidates for and would simply vanish. An author-bearing group's hints
  already reach it through `add_projected_candidates`, inside the solve, where
  they compete for the k=2 slots and earn coverage like any other relay — so
  routing for author-bearing groups is **byte-for-byte master's**.

  An earlier revision ran this lane unconditionally. That was a behaviour
  expansion rather than a consequence of collapsing two routing values: a
  hinted relay got a REQ outside the solve and outside coverage, one member's
  `nevent` hint dragged every sibling's filter along (`routing_evidence` is
  unioned across the group), the durable claim covered every author in the
  group, and with an unbound member the hint relay received the bare skeleton —
  `kind:1` from anyone, no limit. Narrowed, with a falsifier in both directions
  (BREAK D).
- **The bag partition merges.** `bag.entry(routing)` is the coalescing unit, so
  what used to be the outbox lane and the supplemental lane now share one
  partition and may merge. That is intended — they are one strategy now — but
  it is the load-bearing risk of the change, so it is proven rather than argued
  (BREAK C).

### The hazard that merge creates, and how it is handled

An authorless atom and an author-bearing atom can share an author-erased
`Skeleton`, so they land in one group. Routing the additive lanes under the
group's author union would silently narrow "kind:1 from anyone" into "kind:1
from alice" — the app asks for everyone, receives one author, and nothing
reports a loss.

`AutoAtomGroup::unbounded` records that some member left `authors` **unbound**;
the additive lanes then carry the bare skeleton, which supersets every
author-bearing sibling. `auto_ownership` gives that member its own coverage
claim and owner edge, which the per-author walk cannot produce (an empty author
set has nothing to iterate, and `is_disjoint` against it is vacuously true).

`unbound` is read from `atom.filter.authors.is_none()`, deliberately not from
`Skeleton::of`'s empty author set — that reports empty for both `None` and
`Some(∅)`, and those are different demands. "Asked about everyone" is unbound;
"asked about nobody" is not, and gets no claim. `coverage_claim_atoms` already
draws exactly that line, and the two halves of this change must agree about it.
(Unreachable today, since the resolver yields no atom for an empty bound slot,
but the disagreement should not ship.)

**Adversarial review found this accounting sound.** It tried to construct an
over-crediting case — durable coverage minted for authors never served, the
unrecoverable direction — and could not, for structural reasons: the unbounded
claim widens in lockstep with `lane_filter` (one `if` decides both, so they
cannot drift), coverage rows are exact-key lookups so a bare-skeleton credit
never satisfies a narrower key, and `both_constrain` refuses to union an
unbounded operand with a bounded one, so the bare member ships as its own REQ.

## `RelayRequest` reports which lane asked

`ObservationFact::RelayRequest` gained `lanes: BTreeSet<Lane>`, threaded from
the router's `WireReq.provenance` through `PlanExecutionMetadata`,
`RequestSend`, `RequestAttemptState` and `PendingRequestEvidence`, and surfaced
as a `lanes` attribute on the public observation evidence.

This is the accountability half of making `Auto` the default. A default that
decides a route has to report the route it decided, or it is the same
unaccountable magic under a better name — the trace previously said which
relays were asked and never why.

A **set**, not one lane, because coalescing is real: one REQ can be two
authors' outbox lane and the operator's app lane at once, and naming a single
lane would be true but partial. A NIP-77 probe or reconciliation step reports
`none` — no lane asked for it, and that is a statement rather than a missing
value.

## Normalization, and why the digest cannot disagree with identity

`Demand::new` sorts and dedupes an `Explicit` relay set on the way in, so one
routing intent has one representation. Without it, `Explicit([b, a])` and
`Explicit([a, b])` are two atoms, two refcount entries and two wire
subscriptions for what the caller said once.

Separately, `fold_context` folds the `Vec` **in its own order** — the same
order the derived `Ord`/`Hash` read. That is deliberate: the two therefore
agree for every value, normalized or not. An order-*insensitive* digest over an
order-*sensitive* `Eq` is the disagreement worth fearing, because it makes two
atoms that compare unequal share a coverage key. `Auto` folds to
`blake3(base ++ [0])` and `Explicit` to `blake3(base ++ [1] ++ …)`, so the
empty-relay case cannot collide with the no-relay case either.

## Why an enum rather than a bare relay field

A `relays: Option<Vec<RelayUrl>>` field on `Demand`, with absence meaning "NMP
routes it", was considered and rejected on evidence.

`crates/nmp-runtime/tests/handle_surface_guard.rs` is a textual scan of every
non-comment line of `nmp-runtime/src/**/*.rs` for the token `relays:`. It is
not restricted to method signatures despite its message, and
`crates/nmp-runtime/src/lib.rs` constructs an explicit-routed `Demand` inside
that scanned tree.

Simulated faithfully — temporary `pub relays` field, that call site rewritten
as the struct literal the field shape forces — the guard **reddens**:

```
thread 'handle_surface_is_closed_and_receipt_reattachment_is_explicit' panicked at
crates/nmp-runtime/tests/handle_surface_guard.rs:150:5:
no method signature on the runtime surface may take a bare `relays:` parameter
```

Reverted; guard green. Landing the field shape would mean weakening a ledger
#2/#3 guard to accommodate a naming preference. The enum shape does not trip
it (verified green).

The field shape also has two spellings of "no relays" (`None` and
`Some(vec![])`) where the enum has one, which adds an unrepresentable-state
problem rather than removing vocabulary. Note that neither shape closes the
struct-literal hole: `Demand` derives `Default` with all-public fields — that
is what makes `Demand { selection, ..Demand::default() }` the idiom — so
`Explicit(vec![])` is constructible without passing `Demand::new`. 245
struct-literal constructions against 72 `Demand::new` calls in the tree.

## Call sites shrink

188 Rust constructor call sites migrated mechanically; the great majority lose
their routing argument entirely. Swift and Kotlin the same:

```swift
// before
NMPDemand(selection: followFeed, source: .authorOutboxes)
// after
NMPDemand(selection: followFeed)
```

```kotlin
// before
NMPDemand(selection = filter, source = NMPSourceAuthority.AuthorOutboxes)
// after
NMPDemand(selection = filter)
```

Every tier's mirror moved together: `FfiReadRouting { Auto, Explicit }`,
`NMPReadRouting { auto, explicit }`, `NMPReadRouting { Auto, Explicit }`, plus
the error surface (`EmptyExplicitRelaySet`, with the outboxes variant deleted).
Bindings regenerated for both tiers.

## Proven, not believed

Every tier built and run on one machine. `pwd` and `git rev-parse HEAD` were
emitted in the same command as each test invocation so the numbers cannot come
from another tree.

### The suite, reconciled per target

Build system: **Bazel** (`bazel test //...`), authoritative since `36a44d6c`.
Baseline measured, not assumed: a throwaway worktree at `origin/master`
(`0ab58e7c`), same machine, same Bazel cache.

| Tree | Bazel targets | passed | failed | ignored |
|---|---|---|---|---|
| `0ab58e7c` (`origin/master`, rebased parent) | 121 / 121 pass | 2067 | 0 | 10 |
| this branch | 121 / 121 pass | 2077 | 0 | 10 |

Target set **identical** (121 both sides; the only `BUILD.bazel` diff in the
whole change is one blank line, now canonical from
`tools/bazel/gen_buildfiles.py`).

Reconciled per target rather than in aggregate — a diff of the 121-row
`(target, passed, failed, ignored)` table shows exactly four moved rows:

| Target | Before | After | Why |
|---|---|---|---|
| `nmp-grammar:unit_tests` | 68 | 69 | inference tests deleted, routing/normalization tests added |
| `nmp-engine:unit_tests` | 305 | 306 | the lane-reporting falsifier |
| `nmp-router:contract` | 7 | 12 | bag-merge, refcount, hint-lane (x2) and admit-path falsifiers |
| `nmp-router:unit_tests` | 71 | 74 | `classify`/`outbox_authors` and wire-id routing tests |

`+10` total, `0` failed everywhere, `ignored` unchanged. No offsetting errors
hide inside the total.

### Other tiers

| Tier | Command | Result |
|---|---|---|
| Swift | `swift test` (Packages/NMP) | 181 passed, 3 skipped, **0 failures** |
| Kotlin | `./gradlew test` (Packages/NMPKotlin) | BUILD SUCCESSFUL, 119 tests |
| Canary iOS | `xcodebuild -scheme Canary -destination 'generic/platform=iOS Simulator'` after `xcodegen generate` | **BUILD SUCCEEDED** |
| CanaryScenarios | `swift build --build-tests` | clean |
| Lints | `cargo clippy --workspace --all-targets`, `cargo fmt --all --check` | 0 warnings, clean |

`swift test` rather than `swift build` was load-bearing: **~20 bare-filter
`observe` call sites survived in the Canary scenario *test* targets**, which
`swift build` does not compile. They are migrated.

**`fixtures/android-aar-consumer` is UNVERIFIED.** It was migrated by reading
signatures only; it cannot be compiled here because `ANDROID_HOME` points at a
path that does not exist. The change is one import and one constructor
(`NMPSourceAuthority.Pinned(setOf(relay))` → `NMPReadRouting.Explicit(listOf(relay))`).

### The admit path

`compile` sees the whole demand set at once; `admit` compiles one cohort
against an empty incumbent namespace and appends. Both are covered by
`an_authorless_atom_keeps_its_reach_whichever_order_admission_sees_it`, over
three shapes.

It needed **a test, not a fix**, and the reason is worth recording: the two
sequential shapes are structurally immune — a lone unbound atom forms its own
group and never meets a sibling's author set — so breaking the widen leaves
them green. Only **both atoms in one cohort** reproduces the grouping, and that
shape does redden under the break. A test asserting only the sequential orders
would have been false assurance.

### The semantic oracle

`crates/nmp-store/src/semantic_oracle.rs` mints a coverage atom that was
`AuthorOutboxes`. Checked structurally, the way a golden trace should be:

```
checkpoint count: old=41 new=41  SAME
names+order:      IDENTICAL
digests moved:    0 of 41
equal-digest groups: old=4 new=4   preserved: YES
```

Zero digests moved. `Auto` inherited `AuthorOutboxes`'s discriminant byte in
`fold_context` (`fold_byte(base, 0)`), and the oracle's atom was an outbox
atom, so the coverage key is byte-identical. The 41 changed lines in that
file's diff are rustfmt reflow of the const, not value changes — verified by
parsing both sides and comparing the `(name, digest)` pairs, not by reading the
diff.

### BREAK A — the default

`Demand::default()` made to return `Explicit(["wss://wrong-default.example"])`
instead of `Auto`. `cargo test -p nmp-grammar --lib` **reddened** 5 tests,
including the one that owns the property:

```
---- descriptor::tests::a_demand_that_names_no_routing_is_auto stdout ----
assertion `left == right` failed
  left: Explicit([RelayUrl("wss://wrong-default.example")])
 right: Auto
```

Failed for the intended reason. Reverted; 69 / 0.

### BREAK B — the normalization

`Demand::new`'s `relays.sort(); relays.dedup();` removed. **Reddened exactly
two**, the direct one and the identity/digest one:

```
---- descriptor::tests::new_sorts_and_dedupes_an_explicit_relay_set ----
  left: Explicit([b.example, a.example, b.example])
 right: Explicit([a.example, b.example])

---- concrete::tests::one_routing_intent_declared_in_two_orders_is_one_atom ----
assertion `left == right` failed   (two DescriptorHashes)
```

The second is the one that matters: it is the digest disagreeing with itself
across two spellings of one intent. Reverted; clean.

### BREAK C — the bag merge (both halves)

Guard: `an_authorless_demand_is_not_narrowed_by_an_author_bearing_sibling`.

1. `lane_filter`'s `group.unbounded` branch replaced with
   `skeleton.with_authors(authors.clone())` — the naive merge:

   ```
   the operator lane must carry the author-unbound skeleton, not the group's
   author union
   ```

2. `unbounded` replaced with `false` in the lane's `auto_ownership` call — the
   refcount half:

   ```
   the authorless demand must own the request its selection reaches the wire
   through
   ```

Both reddened for their intended reason. Reverted; 9 / 0.

### BREAK D — the direct hint lane, both directions

Guard pair: `an_author_bearing_group_never_reaches_a_hint_relay_outside_the_solve`
and `an_unbound_group_routes_its_hints_directly`.

Making the `if group.unbounded` guard unconditional again reddened the first
and left the second green — the discrimination the narrowing exists for:

```
an author-bearing group's hints belong to the solve; a Supplemental hint route
is the direct lane leaking into a group that never had it
```

The discriminator is exact rather than positional: a hint relay chosen **by the
solve** carries `RouteKind::Coverage`, while the direct lane mints
`RouteKind::Supplemental`. The solver remains free to pick a hint relay on
merit, and that is not what this forbids.

Independent confirmation that the narrowing restores master's behaviour for
author-bearing groups: `differential_oracle` — whose demand is entirely
author-bearing — needed its fixture universe widened to include the harness's
ingest relay while the lane was unconditional, and passes with master's
original universe now that it is narrow. That widening has been reverted.

### BREAK E — the lane reporting

`let lanes = self.router.request_lanes(session, sub_id)…` replaced with
`BTreeSet::new()` at the plan-install site that feeds the observation fact:

```
---- an_accepted_request_reports_the_lane_that_asked_for_it ----
assertion `left == right` failed
  left: [{}]
 right: [{Exact}]
```

Worth recording that the *first* attempt at this break edited the metadata site
instead and the test correctly stayed green — the two paths are genuinely
distinct, and the falsifier is specific to the one that reaches the app.

### A weak test caught and strengthened

The refcount falsifier initially used the same author for its `Auto` and
`Explicit` atoms. Breaking `outbox_authors` so `Explicit` also contributed did
**not** redden it — the census counts distinct authors, so the length was
unchanged. Fixed by giving the `Explicit` atom a different author; the same
break then reddens `left: 2, right: 1`. Recorded because the first version
would have shipped as false assurance.

## Three defects found by running things

1. **`coverage_claim_atoms` returned no claims for an authorless `Auto` atom.**
   The old `Public` branch returned the atom itself; the collapse routed it
   into the outbox branch, which returns empty for an unbound `authors`. An
   authorless live query would have proven no durable coverage and re-fetched
   forever. Caught by Bazel, not by `cargo test`. Fixed, with the unbound case
   (one exact claim) now distinguished from the bound-but-empty case (no
   claims, because nothing was asked of any author).
2. **~20 bare-filter `observe` sites in Canary scenario tests**, invisible to
   `swift build`.
3. **The differential oracle panicked on a relay outside its fixture universe**
   — the unconditional hint lane reaching the harness's own ingest relay. This
   was the first visible symptom of the over-reach that BREAK D now bounds; with
   the lane narrowed, the oracle passes against its original universe.

## Rebase note

Rebased onto `origin/master` `0ab58e7c`, **dropping `f4368397`**. That commit
deleted the orphaned `//crates/nmp:correlation_restart` Bazel target; #1877
landed the same fix on master during this branch's verification, so ours was
redundant and its only remaining difference was one blank line — which
`gen_buildfiles.py` now removes canonically.
