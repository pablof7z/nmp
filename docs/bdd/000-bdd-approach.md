# BDD in NMP — mental model, foundation, exemplars

- **Date:** 2026-07-11
- **Status:** Founding doc for the BDD practice. Scenarios written under it are provisional-until-v2 like everything else; the *practice* itself starts now.
- **Grounding:** `docs/VISION.md` (the two-noun surface, P1–P7), `docs/bug-class-ledger.md` (each entry is a behavior), the proven engine (`nmp-engine/src/runtime/mod.rs` — the `Handle`), and the existing contract/integration tests this practice re-expresses readably (`nmp-engine/tests/*`, `nmp-resolver/tests/contract.rs`, `nmp-router/tests/*`).

---

## Part 1 — The mental model

### 1.1 Why BDD fits this engine in particular

NMP's founding bet is that **correctness lives in the shape of the API, not in a police force patrolling it**. BDD is the same bet applied to *verification*: the behavior contract lives in the shape of the scenario — plain language a product person can read — not in test internals that only the author of the implementation can audit.

Three properties make the fit unusually tight:

1. **The surface is two nouns.** A live query and a write intent, plus identity-as-input and a diagnostics stream. A surface that small has a small behavioral vocabulary — small enough that a *closed catalog* of Given/When/Then steps can cover it, the way the closed `Selector` vocabulary covers derivation. BDD over a 46-verb framework drowns; BDD over two nouns sings.
2. **The diagnostics surface is already "the acceptance test rendered on screen."** VISION §1 says so verbatim. A `Then` clause is the same acceptance test rendered in prose: `DiagnosticsSnapshot` (per-relay `wire_sub_count`, exact `filters`, `by_lane`, `authors_served`, `events_by_kind`, per-filter `coverage`) gives every routing/coverage claim an *observable* to assert against without touching engine interiors. BDD didn't need a new observation seam built for it; the engine's own philosophy already built one.
3. **Scenarios survive rewrites; that is the point of a behavior-first engine.** M1–M4 each rewrote internals under a stable surface. A scenario that says "unfollowing Carol stops her notes and touches nobody else's subscriptions" was true of the resolver spike, is true of the full runtime, and must stay true of every future refactor. It is the durable form of the proof; the contract tests are its current mechanization.

And one social property: the owner, the agents, and (eventually) external builders need **one shared statement of what the engine promises**. Rust contract tests are illegible to two of those three audiences. Scenarios are legible to all three, and double as the builder manual's worked examples.

### 1.2 The domain vocabulary

Scenarios are written in the language of a *person using a Nostr app built on NMP*, plus a thin operator's view of relays. They must read like a product person wrote them.

**Personas and nouns**

| Term | Meaning | Never say instead |
|---|---|---|
| **I / my app** | the app embedding the engine (the `Handle` holder) | `Handle`, `EngineThread` |
| **my account / Alice's account** | a keypair; "logged in as" = it is the active account | `ActivePubkey`, signer registry |
| **Alice, Bob, Carol, Dave…** | other Nostr users (fixture keypairs) | pubkey hex in prose |
| **a feed / a query** | one live query the app opened; "my follows' notes" etc. | `LiveQuery`, `HandleId`, `QueryHandle`, atom |
| **note, follow list, mute list, relay list, profile** | the events (kind:1, 3, 10000, 10002, 0) by their human names | bare kind numbers in prose (kind numbers are fine in diagnostics-flavored `Then`s) |
| **relay "alice-relay" / indexer relay** | a named relay in the scenario's topology; indexers are the two discovery-only relays the operator configures | `RelayUrl` literals, slots, generations |
| **a subscription** | one wire subscription as counted by diagnostics | `SubId`, REQ/CLOSE frames, `WireDelta` |
| **a receipt** | the streaming status of a write intent | `ReceiptSink`, `WriteStatus` variants by Rust name |
| **complete / unknown** | the two coverage words; "the query reports its results are complete" | watermark internals, EOSE, `CoverageInterval` |
| **reconciliation** | negentropy sync, when a scenario must name it | NEG-OPEN, `ProbedRelay` |

**The verbs**

- `Given` — *the world before the app acts.* Relay topology and scripted behavior ("Given relay 'flaky' rejects every event today"), configured operator policy ("Given only two indexer relays are configured"), pre-existing protocol state ("Given Alice's relay list names 'alice-relay' as her write relay", "Given I follow Alice, Bob, and Carol"), prior sessions ("Given my feed fully synced yesterday and the app was closed").
- `When` — *one actor does one thing.* The app: opens/closes a feed, publishes, switches accounts, relaunches offline. Another user: "When Alice publishes a new relay list naming relay X", "When Bob posts a note". The network: "When relay 'alice-relay' drops the connection", "When the network comes back".
- `Then` — *an observable outcome,* one of exactly four kinds (see 1.3): rows on a feed, receipt states, diagnostics facts, coverage words.

The vocabulary is drawn from the PUBLIC behavior and the diagnostics — **never from internals**. Banned tokens in any `.feature` file: `ConcreteFilter`, `SubId`, `HandleId`, `refresh_handle`, `Effect`, `EngineMsg`, `atom`, `EOSE`, `REQ`, `Preamble`, any Rust type name. (This ban is enforced by review, not a lint — consistent with P7. If a scenario cannot be written without an internal name, the *diagnostics surface* is missing an observable, which is a real finding.)

### 1.3 The abstraction level — assert only on observables

Every `Then` clause resolves to one of four observation channels, all of them app-facing:

1. **Rows delivered on a query** — what a feed shows / stops showing / never shows. (The `Receiver<RowsMsg>` a subscribe returns.)
2. **Receipt states** — accepted, signed, sent-to, acked-by, rejected-by, failed. (The `Receiver<WriteStatus>` a publish returns.)
3. **Diagnostics facts** — per relay: how many subscriptions, which filters (as filter *meaning*, e.g. "asks only for relay lists"), which lane, how many events of which kind arrived, per-filter coverage. (The `observe_diagnostics` stream.)
4. **Coverage words** — a query's results are *complete up to* a moment, or *unknown*. (`QueryCoverage` on every row batch.)

Nothing else is admissible. No peeking at the store, no reading resolver graphs, no counting `Effect`s. Two consequences, both deliberate:

- **Scenarios are stable across refactors.** M1's resolver, M3's runtime, and any future rewrite deliver the same rows, receipts, diagnostics, and coverage — so the scenarios carry over unchanged. They are the *contract*; the crate-level tests are the current implementation's proof obligations.
- **Scenarios are documentation.** A builder reading `features/routing/self-bootstrapping-outbox.feature` learns what the engine does for them, in the words they'd use to describe their own app, with the diagnostics screen as the place they'd go verify it themselves.

Timing is handled by the step library, never by the scenario: "Then my feed shows Dave's notes" means *within the step's bounded await*, and "never" means *not within the scenario's settle window after the trigger*. Scenario text stays timeless.

### 1.4 How BDD unifies the rest of NMP

BDD is not a fifth verification mechanism next to the ledger, the contract tests, the smoke apps, and the diagnostics. It is the **readable roof over all four** — each existing mechanism plugs into a scenario at a specific joint:

**(a) Bug-class ledger entries become "must-never" scenarios.** Every ledger entry is already a behavior statement ("stale replaceable retained", "enqueue treated as converged") plus a structural mechanism. The scenario is the *falsification made human-readable*: it stages the situation that would produce the bug and asserts the bug's absence through the four observables. Two flavors, kept honest:

- *Runtime-observable entries* (#1, #2, #4, #5, #7, #8, #9, #10, #11 in part) get a full runnable scenario — e.g. #10 is "switching accounts never leaks the old account's feed" asserted on rows + diagnostics.
- *Compile-time entries* (#3's missing `relays:` parameter, #6's un-widenable `NarrowOnly`, #8's unforgeable `ProbedRelay`) are excluded by types, and a runtime scenario cannot "attempt" what doesn't compile. For these, the scenario asserts the *runtime consequence* ("no relay outside a routed plan is ever contacted", "an unroutable private write fails, and no relay ever receives it") and the ledger's compile-fail falsification test remains the CI-proof of the mechanism itself. **BDD complements the ledger; it never replaces the falsification standard.**

Tag: `@must-never @ledger-N`, so `grep @ledger` gives the ledger→scenario cross-index.

**(b) Each smoke-test app becomes a `.feature` file.** A deep-kernel probe app (the separate mapping effort) is a *manual enactment* of a scenario set: a human drives the falsifier's diagnostics screen and watches the numbers. The `.feature` file is the same script made headless and repeatable — the app proves it on a device with real relays and human judgment; the feature proves it in CI with scripted relays, forever. When a probe app finds a behavior gap (as the M5 dogfooding found the 7112-events-for-39-authors discovery over-delivery, `docs/known-gaps.md`), the *first* artifact of the fix is a failing scenario ("the engine never requests more relay lists than the authors it is resolving"), then the fix, then the scenario stays green.

**(c) The diagnostic surface is the `Then`-clause instrument.** Routing and coverage are invisible-by-design (P4, P6); the diagnostics exist precisely so invisibility stays falsifiable. Every routing scenario reads them. This also creates healthy back-pressure: when a scenario needs an observable the snapshot lacks (e.g. "total distinct relays ever contacted"), the fix is widening the read-only snapshot — never reaching into internals.

**(d) Scenarios feed the builder manual.** The manual's worked examples ARE scenarios: "here is what the engine does when your user unfollows someone" is `reactive-follows.feature`, verbatim, with prose around it. One source of truth per fact — the manual links features, it does not paraphrase them.

---

## Part 2 — The foundation

### 2.1 Tooling decision: the `cucumber` crate, real Gherkin, one World over the real engine

**Decision: adopt [`cucumber`](https://crates.io/crates/cucumber) (cucumber-rs) with plain-text `.feature` files and Rust step definitions. Do NOT build a parallel typed Given/When/Then Rust DSL.**

Justification:

- **Readability is a hard requirement, and only Gherkin meets it.** The audiences are the owner, agents of varying context, and external builders. A typed Rust DSL (`harness.given_follows(&["alice","bob"]).when_publish_kind3(...)`) is readable to exactly the people who can already read `contract.rs` — it adds a third dialect without adding an audience. Plain `.feature` files are readable by all three audiences and diff-reviewable by a product person.
- **The typed layer already exists.** `nmp_resolver::testkit::Harness` + the contract tests *are* the typed behavioral DSL of this codebase, and they are good. BDD's job is the acceptance layer above them, not a re-skin of them.
- **cucumber-rs is a fit, not a stretch.** Pure Rust, no external runtime, runs under `cargo test` via `harness = false`, supports tags (`@live`, `@must-never`), `Scenario Outline` tables (perfect for cap/coverage parameterizations), and a typed `World` per scenario. Its async executor coexists fine with the engine's own OS threads — `Handle` is `Send + Clone` and the existing `runtime_integration.rs` already drives the sync engine from `#[tokio::test(flavor = "multi_thread")]`; step definitions use the same bounded `recv_timeout` awaits that file already established.
- **Gherkin's classic failure mode — step-soup — is neutralized by NMP's own philosophy.** Free-form step invention is the closure trap in prose form. So the step library is a **closed, introspectable vocabulary** (§2.4), exactly like `Selector`: scenario authors compose from the catalog; a behavior the catalog can't express extends the catalog deliberately (a reviewed change), never inlines an ad-hoc step. This one rule is what keeps a Gherkin suite maintainable at year two.
- **Scenarios must run against the REAL engine**, and they do: the World spawns `EngineThread::spawn(...)` — the same code path an app uses — against scripted in-process relays (§2.3). No mocked engine, no resolver-only shortcut. (The resolver testkit's *event builders* are reused as fixture factories; its `Harness` is not the BDD substrate because it stops before the wire.)

### 2.2 Structure

```
features/                                  # repo root — deliberately NOT under any crate:
  queries/                                 #   these are product documentation first
    reactive-follows.feature
    derived-and-setops.feature             # follows − mutes, depth-2 groups
  routing/
    self-bootstrapping-outbox.feature
    coverage-and-caps.feature
    indexers-are-discovery-only.feature
  identity/
    account-switch.feature
    logged-out-and-keyless.feature
  writes/
    receipts.feature
    private-routes-fail-closed.feature
  coverage/
    offline-authoritative.feature
    empty-vs-unknown.feature
  sync/
    negentropy.feature
    reconnect-replay.feature
  diagnostics/
    observability.feature
  must-never/                              # the ledger cross-index (@ledger-N tags)
    never-outside-the-plan.feature
    never-cross-account.feature
    never-enqueue-as-converged.feature
nmp-bdd/                                   # test-only runner crate; never published,
  Cargo.toml                               #   no production crate depends on it
  tests/bdd.rs                             # [[test]] name="bdd", harness=false → cucumber main
  src/world.rs                             # NmpWorld
  src/relays.rs                            # ScriptedRelay registry
  src/steps/{given,when,then}.rs           # THE closed step catalog
```

**`NmpWorld`** (one per scenario, fresh engine, fresh relays):

- `people: HashMap<String, Keys>` — "Alice" → fixture keypair, minted on first mention (builders harvested from `nmp_resolver::testkit`: `kind1`, `kind3`, `kind10000_mutes`, plus `kind10002` from `self_bootstrap_outbox.rs` — promote these into a shared `nmp-fixtures` dev crate rather than copy a third time).
- `relays: HashMap<String, ScriptedRelay>` — "alice-relay" → an in-process relay on an ephemeral port. `ScriptedRelay` wraps the `LocalRelay`/`mock_relay` pattern (`nmp-transport/tests/mock_relay.rs`, `nmp-engine/tests/runtime_integration.rs` — including its free-port + rebind trick for reconnect scenarios and the nostr-crate-version bridging gotcha) with behavior knobs: `seed(events)`, `reject_writes()`, `drop_connections()`, `supports_negentropy(bool)`, and a log of every accepted connection and frame — the *world-side* observable for "never contacted" assertions.
- `engine: Option<(EngineThread, Handle)>` — spawned **lazily on the first `When`**, so all `Given`s (topology, seeds, config) are staged before the engine exists; config carries exactly what an app supplies: store, directory, cap, indexer set.
- `feeds: HashMap<String, Feed>` — named open queries; each `Feed` drains its `Receiver<RowsMsg>` into an accumulated row set + latest coverage (the accumulation helper already exists in `runtime_integration.rs`).
- `receipts`, `diag: LatestReceiver<DiagnosticsSnapshot>` — publish receipts by name; one diagnostics observer opened per scenario at engine spawn.
- **Await discipline:** `Then` steps use `eventually(pred, SETTLE)` / `never(pred, SETTLE)` built on `recv_timeout` (blocking recv with deadline — test-side, D8 governs production code, and this mirrors the existing tests' style). One `SETTLE` constant for the whole suite; scenarios never mention time.

**Two tiers, tagged:**

- **Wire tier (default, CI, every push):** real engine + scripted in-process relays. Deterministic, no network, seconds-fast. Runs via `cargo test -p nmp-bdd` — repo-standard scoped testing applies.
- **Live tier (`@live`, bounded):** the *same steps* against real network relays (the `negentropy_live.rs` / `nmp-demo` lineage). Filtered out by default (`cucumber` tag filter), enabled by `NMP_BDD_LIVE=1`, budget-capped: at most a handful of scenarios (self-bootstrap against real indexers; a durable publish with divergent per-relay terminals), because live relays make flaky oracles. The live tier exists to keep the wire tier honest, not to be the suite.

**Coexistence with existing tests — the layer rule:** BDD is the acceptance layer **on top**. Crate-level unit/contract/property tests (resolver contract tests, router property tests + differential oracle, store tests, ledger compile-fail falsifications) stay exactly where they are and keep their jobs: they prove *mechanisms* at the seam where the mechanism lives, run fast, and localize failures. A scenario failing tells you *which promise* broke; the contract tests tell you *where*. Nothing is deleted or ported "into" BDD; migration (§2.5) means *covering* proven behaviors with scenarios, not moving their tests.

### 2.3 Extending relays' scriptability

The default `ScriptedRelay` is a well-behaved NIP-01 relay. Scenario `Given`s can degrade it per-scenario: reject events (receipt scenarios), never send EOSE (coverage stays `unknown`), advertise/deny negentropy (sync scenarios), drop and re-accept connections (replay scenarios), or over-deliver beyond the filter (the known-gaps discovery finding — the engine-side guard scenario). Relay misbehavior is a `Given` about the world, which is exactly where Gherkin puts it.

### 2.4 The starter step catalog (the closed vocabulary)

Steps are the reusable sentences; parameters in `<>`. Extending this catalog is a reviewed change to `nmp-bdd/src/steps/` — scenario PRs compose, they do not invent.

**Given — the world**
- `only <n> indexer relays are configured` / `relays <list> are configured as indexers`
- `a relay <name>` · `relay <name> rejects every event` · `relay <name> never confirms end of stored events` · `relay <name> supports reconciliation` / `has never been probed for reconciliation`
- `<person>'s relay list names <relay> as their write relay` (seeds kind:10002 where discoverable)
- `I am logged in as <person>'s account` / `I am logged in as an account that follows <people>` / `I am not logged in`
- `<person> follows <people>` · `<person> mutes <people>` · `<person> has posted <n> notes` / `a note saying <text>`
- `my feed of my follows' notes is open` (and the depth-2 variant: `my feed of my groups' activity is open`)
- `my feed fully synced and the app was closed` (runs a session, shuts down, keeps the persisted store)

**When — one actor acts**
- `I open a feed of <shape>` · `I close the feed`
- `I publish a note saying <text>` · `I publish a new follow list <people>` · `I send a private message to <person>`
- `I switch to <person>'s account` · `I log out`
- `<person> posts a note saying <text>` · `<person> publishes a new relay list naming <relay>` · `<person> updates their follow list to <people>`
- `relay <name> drops the connection` · `relay <name> comes back` · `I relaunch the app with no network`

**Then — the four observables**
- rows: `my feed shows <person>'s notes` · `my feed shows the note saying <text>` · `notes from <person> no longer arrive` · `my feed never shows <…>` · `my feed is empty`
- receipts: `the receipt first reports only accepted — never sent` · `the receipt reports the note acked by <relay>` / `rejected by <relay>` · `the write fails with no relay ever receiving it`
- diagnostics: `exactly <n> subscription(s) is open to relay <name>` · `the subscriptions serving <people> are untouched` · `the indexers are asked only for relay lists and profiles` · `no more than <n> relays are contacted in total` · `each followed author is served by at least <n> relays` · `every contacted relay appears in the diagnostics with its routing lane` · `no relay outside <set> was ever contacted` (diagnostics ∪ the ScriptedRelay connection logs) · `reconciliation is used with <relay>` / `<relay> receives a plain subscription`
- coverage: `the query reports its results are complete` / `complete as of the last sync` · `the query reports its results are unknown — not empty`

### 2.5 Migration — BDD what we've got

Order: marquee behaviors first, each mapped to the tests that already prove it (the scenario must go green against today's engine — writing it is transcription plus honesty-checking, not new proof):

| # | Feature file | Behavior | Existing proof it re-expresses |
|---|---|---|---|
| 1 | `queries/reactive-follows.feature` | surgical re-route on follow change; unchanged authors zero churn; stale/duplicate kind:3 inert | `nmp-resolver/tests/contract.rs` tests 1, 4, 5, 6; `nmp-engine/tests/core_headless.rs::ingest_frame_recompiles_wire_and_emits_rows` |
| 2 | `routing/self-bootstrapping-outbox.feature` | 2 indexers → discovered write relays; discovery grows with follows; no content from indexers | `nmp-engine/tests/self_bootstrap_outbox.rs` (all three), `discovery_churn.rs`; `@live`: the `nmp-demo` run |
| 3 | `identity/account-switch.feature` + `must-never/never-cross-account.feature` | re-root; old demand closed before new; reads+writes move together (ledger #10) | resolver contract test 3; `signer_registry_headless.rs` (both) |
| 4 | `writes/receipts.feature` + `must-never/never-enqueue-as-converged.feature` | receipt streams; per-relay divergent terminals; keyless publish fails closed (ledger #9) | `core_headless.rs::enqueue_is_not_converged`, `::write_ack_per_relay`, `::private_route_fails_closed`; `runtime_integration.rs` live publish |
| 5 | `coverage/offline-authoritative.feature` + `empty-vs-unknown.feature` | cold-start offline complete; authoritative empty; limit-poison; unknown ≠ empty (ledger #7) | `integration_capstone.rs`; `core_headless.rs` coverage tests (315–545) |
| 6 | `routing/coverage-and-caps.feature` | 2-relay-min; cap under adversarial mailboxes; indexer lane discovery-only (ledger #4) | `nmp-router/tests/contract.rs` coverage + lane tests |
| 7 | `sync/negentropy.feature` + `reconnect-replay.feature` | unprobed → plain REQ (ledger #8); reconnect resumes with no gap | `core_headless.rs` negentropy block (832–1010); `runtime_integration.rs` reconnect; `negentropy_live.rs` (`@live`) |
| 8 | `diagnostics/observability.feature` | snapshot reports real subs/filters/kind-counts; coverage flips unknown→complete reactively | `diagnostics_headless.rs` (both) |
| 9 | `must-never/never-outside-the-plan.feature` | no connection outside a solver plan (ledger #3/#4 runtime consequence) | new assertion (ScriptedRelay logs + diagnostics) over existing behavior |

Then: one feature file per smoke-test app as that mapping lands, and a scenario-first rule going forward — every new engine behavior and every known-gaps closure lands with its scenario (e.g. the discovery over-delivery gap in `known-gaps.md` gets its guard scenario *before* the fix).

---

## Part 3 — Exemplar scenarios (the templates)

These are the house style: title states the promise; `Given`s stage world-topology → protocol-state → app-state in that order; one `When` actor; `Then`s only in the four observables; tags carry tier and ledger links.

```gherkin
Feature: My feed follows my follow list
  The engine keeps "my follows' notes" correct forever. The app declares the
  feed once; it never watches follow lists or manages subscriptions.

  Scenario: Unfollowing one person touches only that person's subscriptions
    Given I am logged in as an account that follows Alice, Bob, and Carol
    And my feed of my follows' notes is open
    When I publish a new follow list with Alice, Bob, and Dave
    Then my feed shows Dave's notes
    And notes from Carol no longer arrive
    And the subscriptions serving Alice and Bob are untouched

  Scenario: An out-of-date follow list changes nothing
    Given I am logged in as an account that follows Alice and Bob
    And my feed of my follows' notes is open
    When a relay delivers an older version of my follow list naming only Carol
    Then my feed still shows Alice's and Bob's notes
    And no subscription is opened for Carol
```

```gherkin
Feature: The engine finds everyone's relays on its own
  Given nothing but two indexer relays, the engine discovers where each
  followed author actually writes and fetches content there. The app
  resolves no relays — there is nowhere to even pass one in.

  Scenario: Content is fetched from the author's own write relay
    Given only two indexer relays are configured
    And Alice's relay list names "alice-relay" as her write relay
    And I am logged in as an account that follows Alice
    When I open a feed of my follows' notes
    Then the indexers are asked only for relay lists and profiles
    And Alice's notes arrive from "alice-relay"
    And no relay outside the two indexers and "alice-relay" was ever contacted
```

```gherkin
Feature: Enough relays to be safe, never a flood
  @ledger-4
  Scenario Outline: Every author is read from at least two relays, under a cap
    Given I am logged in as an account that follows <authors> people
    And every followed author's relay list is known
    When I open a feed of my follows' notes
    Then each followed author is served by at least 2 relays
    And no more than <cap> relays are contacted in total

    Examples:
      | authors | cap |
      | 5       | 10  |
      | 50      | 15  |
```

```gherkin
Feature: Switching accounts is clean
  @must-never @ledger-10
  Scenario: The old account's feed cannot leak into the new one
    Given I am logged in as Alice's account
    And my feed of my follows' notes is open
    When I switch to Bob's account
    Then every subscription serving Alice's account is closed before any of Bob's open
    And my feed shows only notes from Bob's follows
    And notes from Alice's follows never arrive after the switch
```

```gherkin
Feature: What the cache says offline is the truth
  @ledger-7
  Scenario: A synced feed is trustworthy with no network at all
    Given my feed of my follows' notes fully synced and the app was closed
    When I relaunch the app with no network
    Then my feed shows the previously synced notes immediately
    And the query reports its results are complete as of the last sync

  Scenario: Emptiness is only claimed when it is proven
    Given a fresh app that has never synced
    When I open a feed of my follows' notes with no network
    Then my feed is empty
    And the query reports its results are unknown — not empty
```

```gherkin
Feature: Publishing tells the truth, per relay
  @ledger-9
  Scenario: One note, two relays, two different answers
    Given my relay list names "good-relay" and "flaky-relay" as my write relays
    And relay "flaky-relay" rejects every event
    When I publish a note saying "hello"
    Then the receipt first reports only accepted — never sent
    And the receipt reports the note acked by "good-relay"
    And the receipt reports the note rejected by "flaky-relay"
```

```gherkin
Feature: Fancy sync only where it is proven to work
  @ledger-8
  Scenario: An unprobed relay gets a plain subscription
    Given relay "modern-relay" supports reconciliation
    And relay "legacy-relay" has never been probed for reconciliation
    And both serve authors I follow
    When I open a feed of my follows' notes
    Then reconciliation is used with "modern-relay"
    And "legacy-relay" receives a plain subscription
```

```gherkin
Feature: The engine never talks to a relay it has no reason to
  @must-never @ledger-3 @ledger-4
  Scenario: Every connection traces to a routing decision
    Given only two indexer relays are configured
    And my follows' relay lists name "relay-a" and "relay-b"
    And a relay "bystander" exists that nothing references
    When my feed of my follows' notes runs to a steady state
    Then every contacted relay appears in the diagnostics with its routing lane
    And no relay outside the indexers, "relay-a", and "relay-b" was ever contacted
    And relay "bystander" received no connection at all
```

---

## Appendix — writing-agent checklist

1. Compose from the step catalog (§2.4). Need a new sentence? Extend `nmp-bdd/src/steps/` in the same PR, as a reviewed vocabulary change.
2. No internal names in `.feature` files (§1.2 ban list). If you can't say it without one, the diagnostics surface is missing an observable — file that.
3. `Then` = the four observables only (§1.3). No time words in scenarios; the step library owns settling.
4. Tag ledger scenarios `@must-never @ledger-N`; tag network scenarios `@live` (default-off, budget-capped).
5. A scenario for existing behavior must pass against today's engine before merge; a scenario for a bug/gap merges red-then-green with the fix, never red-and-narrated.
6. One promise per scenario title; a product person should be able to read the file top to bottom and nod.
