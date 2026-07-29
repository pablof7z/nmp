Feature: One subscription per relay, not one per thing you asked about
  A query that resolves to many values -- every group I administer, every
  channel in a catalog, everyone whose replies I follow -- must reach a relay
  as a handful of subscriptions carrying large value arrays, never as one
  subscription per value. Relays cap concurrent subscriptions at around 20;
  they accept arrays of 500 values without complaint. Fanning out inverts
  that.

  NMP got this right on ONE axis first. Two demands differing only in
  `authors` merged into a single accumulating filter on a single
  subscription, widened in place as more authors resolved and shrunk in place
  as they went away. The TAG axis had no merge rule at all, so N demands
  differing only in one single-letter tag value produced N subscriptions
  carrying one value each.

  Both axes are now instances of one rule
  (`nmp_router::coalesce::StructuralUnion`): filters differing in exactly one
  ARRAY component -- kinds, authors, ids, or the values under ONE tag name --
  union that component. Scalars must match, a `limit` refuses, and a
  per-filter value bound chunks rather than truncates. Which slot a value set
  occupies stopped being something the wire can tell.

  Every scenario here reads the REQ and CLOSE frames NMP actually put on the
  relay's socket -- subscription ids included -- never the engine's own
  account of what it did.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly

  # ---- the tag axis ----------------------------------------------------

  Scenario: Two values of one tag are one subscription, not two
    # THE headline gap, now closed. Measured live against `nak serve` and
    # again here: `{#p:["alice"]}` and `{#p:["bob"]}` used to reach the relay
    # as TWO subscriptions carrying one value each, where the author axis with
    # the same two demands sent one carrying both. They are the same case --
    # exactly one array component differs -- and one rule now says so.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  Scenario Outline: The collapse is the same for every single-letter tag
    # The contract is tag-name-agnostic: a value list under ANY single-letter
    # tag is a choice between values, so any two demands differing only there
    # are mergeable. All five tags below fanned out identically before the fix
    # (two subscriptions, one value each), so nothing about this was ever
    # specific to `#p` or to the `#d` shape that first surfaced it -- and the
    # rule that fixed it names no tag at all.
    When I watch for notes tagged "<tag>" as "first-value"
    And I watch for notes tagged "<tag>" as "second-value"
    Then relay "hub" serves every "<tag>" watch with 1 subscription
    And one subscription on relay "hub" asks for every "<tag>" value I watch

    Examples:
      | tag |
      | p   |
      | d   |
      | e   |
      | t   |
      | a   |

  Scenario: A third value widens the same subscription in place
    # Growth must cost ONE replacing REQ on the live subscription id, never a
    # close-and-reopen. This needed BOTH halves of the design and neither
    # alone: a merge rule without stable identity paid a Close plus a Req per
    # value (`crates/nmp-router/tests/tag_fanout_churn.rs` measured that),
    # because `SubId::for_wire` keyed on a skeleton erasing `authors` and
    # nothing else. Allocated tokens matched by structural signature (#899)
    # decide continuity from the filter's shape instead, so a grown value set
    # is a one-component difference that overwrites in place.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes tagged "p" as "carol"
    Then relay "hub" serves every "p" watch with 1 subscription
    And relay "hub" widened the "p" subscription in place
    And relay "hub" was never asked to close a "p" subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  Scenario: Dropping one of three shrinks the subscription in place
    # The mirror of growth, and something the author axis already did: the
    # live probe shows an eight-author filter shrinking one author at a time
    # on a single subscription id, never a CLOSE. The tag axis used to hold
    # three separate subscriptions, and dropping one closed it. A shrinking
    # value set is a one-component difference too, so it replaces in place --
    # explicit closure is for demand that is GONE, not merely narrower.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes tagged "p" as "carol"
    And I stop watching notes tagged "p" as "carol"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch
    And relay "hub" was never asked to close a "p" subscription

  Scenario: Values arriving apart in time aggregate like values arriving together
    # Aggregation must not be a debounce window. It already is not one --
    # opening at gaps of 0ms, 50ms and 250ms produces byte-identical wire
    # traffic, because every demand mutation recompiles the whole live demand
    # set. The collapse is a property of that recompile, not of a window; this
    # scenario exists so that no future change can quietly buy the collapse by
    # introducing a batching delay.
    When I watch for notes tagged "p" as "alice"
    And 250ms later I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  Scenario: Collapsing does not wait for the relay to finish
    # Resolution is driven by ingested rows alone, so a relay that never
    # confirms end of stored events must not hold up a value already known.
    # A never-confirming relay produces the same wire traffic as a
    # well-behaved one. The concern this pins is that nothing may start gating
    # the collapse on EOSE to get it -- the engine-level twin is
    # `crates/nmp/tests/core_headless/derived_tag_fanout.rs`'s D.
    Given relay "hub" never confirms end of stored events
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  Scenario: Past the value bound the request splits rather than truncating
    # The bound is a frame-size limit, not a demand limit: it must chunk and
    # ship the remainder as further subscriptions, exactly as the id-array
    # bound does (`nmp_router::MAX_IDS_PER_FILTER`, which shards rather than
    # drops). The BDD presents the 1200 values through 21 independent app
    # watches: without coalescing that is still one subscription over the
    # relay ceiling of 20, while avoiding 1200 synchronous whole-plan
    # recompiles in the test harness (#994). The router's direct singleton-
    # atom falsifier separately feeds all 1200 entries one-by-one.
    #
    # THE COUNT IS DELIBERATELY A BOUND, NOT A NUMBER, and this scenario was
    # revised to say so. It originally asked for exactly 3 -- the arithmetic
    # of 1200 at 500 a filter. The coalescer is a greedy pairwise fixed point,
    # so mutually-mergeable inputs double until their union reaches the cap.
    # What is provable rather than emergent is a window -- a terminal state
    # has no mergeable pair, so every pair of chunks sums over the bound and
    # at most one chunk is half-full. The contract worth stating is the
    # relay's own ceiling, which is what this now asserts; the wasted headroom
    # is a bin-packing cost, not a correctness one, and is recorded in
    # `docs/internals/subscriptions/identity-grouping-and-limits.md` §3.3.
    When I watch for notes tagged "p" as 1200 different values
    Then relay "hub" serves every "p" watch with at most 20 subscriptions
    And no subscription on relay "hub" carries more than 500 "p" values
    And every "p" value I watch is covered by some subscription on relay "hub"

  # ---- what must NOT collapse -----------------------------------------

  Scenario: Two different tag names never merge into one request
    # Within one tag name a value list is a CHOICE; across tag names a filter
    # is a CONJUNCTION. Merging "#p is alice" with "#t is nostr" would demand
    # both tags at once and match neither original watch. This already holds,
    # and is here so a TagValueUnion rule cannot be written that overreaches
    # into it.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "t" as "nostr"
    Then relay "hub" serves every "p" watch with 1 subscription
    And relay "hub" serves every "t" watch with 1 subscription
    And relay "hub" never received a request naming both "p" and "t"

  Scenario: A limited watch never merges into an unlimited one
    # A relay-side `limit` caps the RESULT COUNT, not the predicate. Two
    # `limit:10` REQs for different values each promise up to 10 rows; a
    # merged one still promises 10 in total, so the union silently
    # under-fetches. The refusal is therefore on the PRESENCE of a limit, not
    # on limits differing -- two watches with the SAME limit must stay apart
    # too, which is what this asserts.
    When I watch for the latest 10 notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 2 subscriptions
    And every "p" value I watch is covered by some subscription on relay "hub"

  Scenario: A limited watch and an unlimited one for the same tag stay apart
    # The asymmetric case: one side bounded, one not. Folding the bounded one
    # into the unbounded one would drop its bound; folding the other way would
    # truncate the unbounded one. Neither is a widening, so they ship as two.
    When I watch for notes tagged "p" as "alice"
    And I watch for the latest 10 notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 2 subscriptions
    And every "p" value I watch is covered by some subscription on relay "hub"

  Scenario: Two different time windows over one tag never merge
    # `since` is a co-pinned BOUND, not a value list. There is no union of two
    # windows that both widens and stays near either operand: taking the
    # earlier bound over-fetches unboundedly, taking the later one drops
    # events the earlier watch asked for. So a scalar must MATCH for two
    # filters to merge, and two windows over the same tag are two
    # subscriptions however much their values overlap.
    When I watch for notes tagged "p" as "alice" from the last 1 day
    And I watch for notes tagged "p" as "bob" from the last 30 days
    Then relay "hub" serves every "p" watch with 2 subscriptions
    And every "p" value I watch is covered by some subscription on relay "hub"

  Scenario: Two values under one window do collapse
    # The control for the scenario above, and the guard against fixing it by
    # over-refusing: a shared window is a MATCHING scalar, so the two watches
    # differ in exactly one array component and must still collapse. Without
    # this, "refuse anything with a since" would pass the window scenario and
    # nobody would notice.
    When I watch for notes tagged "p" as "alice" from the last 7 days
    And I watch for notes tagged "p" as "bob" from the last 7 days
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  Scenario: A tag watch and an author watch never merge into one request
    # Tag names are conjunctive with each other and with an author list alike:
    # a filter naming both `#p` and `authors` demands both at once and matches
    # neither original watch. This is the ACROSS-AXIS form of the two-tag-name
    # refusal above, and it is what a rule unioning two components at a time
    # would break.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes from Bob
    Then relay "hub" serves every "p" watch with 1 subscription
    And relay "hub" serves every author watch with 1 subscription
    And relay "hub" never received a request naming both "p" and authors

  Scenario: Two demands that cannot be merged never collapse onto one subscription
    # The injectivity requirement. A relay-side `limit` caps the result COUNT
    # rather than the predicate, so the union correctly refuses to widen
    # across one -- and this used to be where demand SILENTLY VANISHED,
    # because `SubId::for_wire` erased `authors` from the skeleton regardless,
    # landing both demands on the same subscription id where one REQ replaced
    # the other forever. Two unmergeable filters now simply get two allocated
    # tokens (#899). Falsifier:
    # `crates/nmp-router/tests/tag_fanout_churn.rs`'s
    # `limited_identical_except_authors_atoms_each_reach_the_wire`.
    When I watch for the latest 10 notes from Alice
    And I watch for the latest 10 notes from Bob
    Then relay "hub" serves every author watch with 2 subscriptions
    And every author I watch is covered by some subscription on relay "hub"

  Scenario: Asking about nothing asks for nothing
    # The privacy floor. An empty resolved value set must never widen into an
    # unfiltered request -- "which groups am I an admin of" resolving to
    # nothing must not become "send me every group's state". This already
    # holds: no subscription is opened at all.
    Given I administer no groups
    When I open the group state of every group I administer
    Then relay "hub" serves every "d" watch with 0 subscriptions
    And relay "hub" was never asked for everything of a kind

  # ---- the axis that already works, as a regression guard --------------

  Scenario: The author axis already does all of this
    # Not aspiration -- this is the shape the tag axis has to match, and the
    # calibration for everything above. Three author watches reach the relay
    # as ONE accumulating subscription, widened in place, with no CLOSE. If
    # this scenario ever fails, the harness is wrong before the engine is.
    When I watch for notes from Alice
    And I watch for notes from Bob
    And I watch for notes from Carol
    Then relay "hub" serves every author watch with 1 subscription
    And one subscription on relay "hub" asks for every author I watch
    And relay "hub" widened the author subscription in place
    And relay "hub" was never asked to close an author subscription

  Scenario: Dropping one author shrinks the author subscription in place
    # The teardown half of the same guard, observed live: an author filter
    # shrinks one value at a time on the SAME subscription id.
    When I watch for notes from Alice
    And I watch for notes from Bob
    And I watch for notes from Carol
    And I stop watching notes from Carol
    Then relay "hub" serves every author watch with 1 subscription
    And one subscription on relay "hub" asks for every author I watch
    And relay "hub" was never asked to close an author subscription

  # ---- the catalog shape this came from --------------------------------

  Scenario: A catalog of groups is one subscription, not three hundred
    # The originating case: "which groups am I an admin of" projected into
    # the `#d` slot of "hydrate all state for those groups". This compiled to
    # 300 subscriptions PER HOST against a ceiling of 20, while every filter
    # carried 1 value out of a 500-value budget; the same demand now compiles
    # to ONE per host carrying all 300
    # (`crates/nmp-router/tests/tag_kill_measurement.rs`, whose kill assertion
    # was inverted to prove it). Nothing about the derived binding was ever
    # the problem -- the same collapse was missing for literal values, above.
    Given I administer 300 groups
    When I open the group state of every group I administer
    Then relay "hub" serves every "d" watch with 1 subscription
    And no subscription on relay "hub" carries more than 500 "d" values
    And every "d" value I watch is covered by some subscription on relay "hub"

  @wip
  Scenario: Learning about one more group replaces the subscription in place
    # THE BEHAVIOUR HOLDS; THIS SCENARIO DOES NOT STAY GREEN. Kept @wip on
    # measured evidence, with the contract pinned elsewhere rather than
    # dropped.
    #
    # A newly resolved value must widen the live subscription, not open
    # another one. That is proven deterministically, for this exact shape, by
    # `crates/nmp/tests/core_headless/derived_tag_fanout.rs`'s
    # `b2_one_more_value_after_a_warm_set_replaces_in_place` -- a warm set of
    # five, then a sixth arriving live, measured as `opened: 0, replaced: 1,
    # closed: 0` against a real `EngineCore`. That test was written FOR this
    # scenario's failure and is the better artefact: it covers the join
    # between the cold-growth ledger (A) and the warm-cache ledger (B), which
    # nothing covered before.
    #
    # Against the live harness the same sequence intermittently shows two
    # distinct `#d` subscription ids with no reuse, or one CLOSE. Measured
    # across eight consecutive suite runs, before and after making the wire
    # assertions poll, and after fixing a genuine ordering race in the fixture
    # (the sixth group used to be able to land before the first outer REQ
    # existed, making the replacement unobservable). The residue is the same
    # class as the author-axis flakes recorded in
    # `docs/internals/subscriptions/identity-grouping-and-limits.md` §8.1c:
    # a one-shot read of an asynchronous channel, and a recompile boundary the
    # harness cannot await. Shipping it green would be luck, not proof.
    Given I administer 5 groups
    And the group state of every group I administer is open
    When I am made an admin of one more group
    Then relay "hub" serves every "d" watch with 1 subscription
    And relay "hub" widened the "d" subscription in place
    And relay "hub" was never asked to close a "d" subscription
    And every "d" value I watch is covered by some subscription on relay "hub"

  @wip
  Scenario: Nothing already asked for is asked for again
    # STILL OPEN, and confirmed independent of the collapse: recompiling
    # re-sends REQs whose subscription id AND filter are byte-identical to
    # what is already live. Re-measured after the collapse landed: the two tag
    # watches below now cost ONE redundant REQ (the widened
    # `{#p:[alice,bob]}` filter, sent twice) rather than two, so the collapse
    # reduced this without removing it. It is `apply_replay` resending
    # `EngineCore`'s full req list on `RelayConnected`
    # (`docs/internals/subscriptions/identity-grouping-and-limits.md` §5.4),
    # which is wire cost on top of the fan-out rather than a consequence of
    # it, and it needs its own fix.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" was never asked for the same thing twice

  @wip
  Scenario: A catalog already on disk opens as one subscription immediately
    # Genuine gap in the HARNESS, not merely in the engine, and scoped out
    # rather than faked green: `NmpWorld` builds every scenario a fresh
    # `MemoryStore`, so there is no vocabulary for "already stored locally"
    # and the two steps below do not exist in the catalog. The behavior a
    # warm cache must have -- resolve the whole set in a SINGLE recompile,
    # and emit one subscription rather than one per value -- is measured
    # instead in `crates/nmp/tests/core_headless/derived_tag_fanout.rs`,
    # which shows the single recompile already holds and the fan-out inside
    # it does not.
    Given I administer 40 groups
    And the group state of every group I administer is already stored locally
    When I open the group state of every group I administer
    Then relay "hub" serves every "d" watch with 1 subscription
    And one subscription on relay "hub" asks for every "d" value I watch

  # ---------------------------------------------------------------------
  # Half two: deltas. Values that already have coverage must not be asked
  # for again, even at the cost of a second subscription.
  # ---------------------------------------------------------------------

  @wip
  Scenario: Growth discovered from a relay is asked for as a delta, not a re-ask
    # The owner's worked example, verbatim, for a feed of notes mentioning
    # the people I follow -- where the set of people is itself discovered by
    # a second query. Each relay finishing its stored events may reveal
    # people the others did not know about. Asking again for the people
    # already covered would make the host re-scan and re-send everything.
    # Measured today: values arriving from two different relays fan out per
    # VALUE, one subscription each, with no notion of what is already
    # covered -- `e_values_arriving_across_two_relays_fan_out_per_value_
    # not_per_relay`.
    Given my follow list is already stored locally and names Alice, Bob, Carol, Dave, and Erin
    When I open a feed of notes that mention the people I follow
    Then "host-relay" is asked about all five of them in one request
    When "indexer-a" finishes its stored events, and its copy of my follow list also names Frank
    Then "host-relay" is asked about Frank
    And "host-relay" is not asked about Alice, Bob, Carol, Dave, or Erin again
    When "indexer-b" finishes its stored events, and its copy also names Grace and Heidi
    Then "host-relay" is asked about Grace and Heidi
    And "host-relay" is not asked about Frank again

  @wip
  Scenario: A delta does not cost a subscription per person forever
    # The other half of the tension: deltas must not become the fan-out by
    # another name. Whatever bounds subscription count in the aggregation
    # scenarios above has to keep bounding it as deltas accumulate, without
    # ever re-asking for a person already covered. Both halves at once, and
    # deliberately silent on how.
    Given my follow list is already stored locally and names 20 people
    And a feed of notes that mention the people I follow is open
    When 30 relays each finish their stored events naming one person nobody else named
    Then the host serves the whole feed with at most 3 subscriptions
    And every one of the 50 people is covered by some subscription
    And no person is ever asked for twice

  # ---------------------------------------------------------------------
  # OPEN DECISION -- not settled. Do not read the alternatives below as the
  # spec; exactly one of them (or something else) will become it.
  #
  # A relay finishes late and reports a SMALLER answer than the one already
  # in flight -- I unfollowed people, or was removed from groups, and this
  # relay is the first to say so. Growth has an obvious answer (ask for the
  # difference). Shrinkage does not: the values that went away are spread
  # across requests that also carry values which are still live, so there is
  # no request that can simply be dropped.
  #
  # The owner's tentative preference is "shut down all the incorrect
  # subscriptions and open a single correct one", but explicitly not sure.
  # The scenario immediately below asserts only what must hold under EVERY
  # candidate; the two after it spell out the candidates so the choice is
  # legible and can be decided on its merits.
  # ---------------------------------------------------------------------

  @wip
  Scenario: A shrinking answer serves nobody stale and drops nobody live
    # The floor. True under every candidate policy, and the only thing this
    # file asserts about shrinkage until the decision is made.
    Given a feed of notes that mention the people I follow is open
    And it is being served for Alice, Bob, Carol, Dave, and Erin
    When "slow-indexer" finishes its stored events, and its copy of my follow list names only Carol and Dave
    And that copy is the newer one
    Then notes mentioning Alice, Bob, or Erin no longer arrive
    And notes mentioning Carol and Dave keep arriving without interruption
    And the host holds no subscription that still asks about Alice, Bob, or Erin

  @wip
  Scenario: [Alternative A, pending decision] Tear it all down and open one correct request
    # The owner's tentative preference. Simple to reason about and trivially
    # correct; the cost is that Carol and Dave get re-scanned and re-served,
    # which is exactly what the delta model exists to avoid. Cheap when the
    # surviving set is small, expensive when one value out of hundreds went
    # away.
    Given a feed of notes that mention the people I follow is open
    And it is being served for Alice, Bob, Carol, Dave, and Erin
    When "slow-indexer" finishes its stored events, and its copy of my follow list names only Carol and Dave
    And that copy is the newer one
    Then every subscription serving that feed is closed
    And one request replaces them, asking about Carol and Dave

  @wip
  Scenario: [Alternative B, pending decision] Replace only what carried a departed value
    # Preserves the delta property for requests that were already correct,
    # at the cost of a more intricate rule and of still re-serving whichever
    # survivors shared a request with a departed value.
    Given a feed of notes that mention the people I follow is open
    And Alice, Bob, and Carol are served by one subscription
    And Dave and Erin are served by another
    When "slow-indexer" finishes its stored events, and its copy of my follow list names only Carol and Dave
    And that copy is the newer one
    Then the subscription serving Dave and Erin is replaced, asking about Dave alone
    And the subscription serving Alice, Bob, and Carol is replaced, asking about Carol alone
    And no subscription that was already correct is disturbed
