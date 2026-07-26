Feature: One subscription per relay, not one per thing you asked about
  A query that resolves to many values -- every group I administer, every
  channel in a catalog, everyone whose replies I follow -- must reach a relay
  as a handful of subscriptions carrying large value arrays, never as one
  subscription per value. Relays cap concurrent subscriptions at around 20;
  they accept arrays of 500 values without complaint. Fanning out inverts
  that.

  NMP already gets this right on ONE axis. Two demands differing only in
  `authors` are merged by `AuthorUnion` into a single accumulating filter on
  a single subscription, widened in place as more authors resolve and shrunk
  in place as they go away. The TAG axis has no merge rule at all
  (`nmp_router::coalesce::RuleRegistry::default_widen_only` is AuthorUnion,
  KindUnion, IdUnion), so N demands differing only in one single-letter tag
  value produce N subscriptions carrying one value each.

  Every scenario here reads the REQ and CLOSE frames NMP actually put on the
  relay's socket -- subscription ids included -- never the engine's own
  account of what it did.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly

  # ---- the tag axis ----------------------------------------------------

  @wip
  Scenario: Two values of one tag are one subscription, not two
    # THE headline gap. Measured live against `nak serve` and again here:
    # `{#p:["alice"]}` and `{#p:["bob"]}` reach the relay as TWO subscriptions
    # carrying one value each (four REQs in all), where the author axis with
    # the same two demands sends one subscription carrying both. There is no
    # TagValueUnion rule in `crates/nmp-router/src/coalesce.rs`, and
    # `crates/nmp-router/tests/tag_kill_measurement.rs` shows `dedup_only()`
    # and `default_widen_only()` give IDENTICAL results on this axis -- the
    # registry is a no-op here.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  @wip
  Scenario Outline: The collapse is the same for every single-letter tag
    # The contract is tag-name-agnostic: a value list under ANY single-letter
    # tag is a choice between values, so any two demands differing only there
    # are mergeable. Measured: all five tags below fan out identically today
    # (two subscriptions, one value each), so nothing about this is specific
    # to `#p` or to the `#d` shape that first surfaced it.
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

  @wip
  Scenario: A third value widens the same subscription in place
    # Growth must cost ONE replacing REQ on the live subscription id, never a
    # close-and-reopen. Measured today: three subscriptions, six REQs, nothing
    # widened. Note that a naive widened filter would NOT fix this on its own
    # -- `crates/nmp-router/tests/tag_fanout_churn.rs` shows each growth step
    # costing a Close plus a Req, because `SubId::for_wire` keys on
    # `route::Skeleton`, which erases `authors` and nothing else. The rule
    # needs the matching skeleton erasure to reproduce the author axis.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes tagged "p" as "carol"
    Then relay "hub" serves every "p" watch with 1 subscription
    And relay "hub" widened that subscription in place
    And one subscription on relay "hub" asks for every "p" value I watch

  @wip
  Scenario: Dropping one of three shrinks the subscription in place
    # The mirror of growth, and something the author axis already does: the
    # live probe shows an eight-author filter shrinking one author at a time
    # on a single subscription id, never a CLOSE. Measured on the tag axis
    # today: three separate subscriptions, and dropping one closes it.
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    And I watch for notes tagged "p" as "carol"
    And I stop watching notes tagged "p" as "carol"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch
    And relay "hub" was never asked to close a subscription

  @wip
  Scenario: Values arriving apart in time aggregate like values arriving together
    # Aggregation must not be a debounce window. It already is not one --
    # opening at gaps of 0ms, 50ms and 250ms produces byte-identical wire
    # traffic, because every demand mutation recompiles the whole live demand
    # set. What fails here is the collapse itself, identically at every gap;
    # this scenario exists so that a fix cannot quietly buy the collapse by
    # introducing a batching delay.
    When I watch for notes tagged "p" as "alice"
    And 250ms later I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  @wip
  Scenario: Collapsing does not wait for the relay to finish
    # Resolution is driven by ingested rows alone, so a relay that never
    # confirms end of stored events must not hold up a value already known.
    # Measured: a never-confirming relay produces the same wire traffic as a
    # well-behaved one -- the same fan-out, no extra churn. The concern is
    # that a fix must not start gating the collapse on EOSE to get it.
    Given relay "hub" never confirms end of stored events
    When I watch for notes tagged "p" as "alice"
    And I watch for notes tagged "p" as "bob"
    Then relay "hub" serves every "p" watch with 1 subscription
    And one subscription on relay "hub" asks for every "p" value I watch

  @wip
  Scenario: Past the value bound the request splits rather than truncating
    # The bound is a frame-size limit, not a demand limit: it must chunk and
    # ship the remainder as further subscriptions, exactly as the existing
    # id-array bound does (`nmp_router::coalesce::MAX_IDS_PER_FILTER`, which
    # shards rather than drops). Measured today: 1200 values compile to 1200
    # subscriptions against a relay ceiling of about 20 -- every filter
    # carrying one value out of a 500-value budget.
    When I watch for notes tagged "p" as 1200 different values
    Then relay "hub" serves every "p" watch with 3 subscriptions
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

  @wip
  Scenario: Two demands that cannot be merged never collapse onto one subscription
    # The injectivity requirement, and a CONFIRMED live bug rather than a
    # missing feature. A relay-side `limit` caps the result COUNT, so
    # `AuthorUnion` correctly refuses to widen across one -- but
    # `SubId::for_wire` erases `authors` from the skeleton regardless, so both
    # demands land on the SAME subscription id and one REQ silently replaces
    # the other, forever. Measured here: two limited author watches produce
    # ONE subscription carrying one author. Falsifier:
    # `crates/nmp-router/tests/tag_fanout_churn.rs`'s
    # `limited_identical_except_authors_atoms_collide_on_sub_id`.
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
    And relay "hub" widened that subscription in place
    And relay "hub" was never asked to close a subscription

  Scenario: Dropping one author shrinks the author subscription in place
    # The teardown half of the same guard, observed live: an author filter
    # shrinks one value at a time on the SAME subscription id.
    When I watch for notes from Alice
    And I watch for notes from Bob
    And I watch for notes from Carol
    And I stop watching notes from Carol
    Then relay "hub" serves every author watch with 1 subscription
    And one subscription on relay "hub" asks for every author I watch
    And relay "hub" was never asked to close a subscription

  # ---- the catalog shape this came from --------------------------------

  @wip
  Scenario: A catalog of groups is one subscription, not three hundred
    # The originating case: "which groups am I an admin of" projected into
    # the `#d` slot of "hydrate all state for those groups". Measured today
    # in `crates/nmp-router/tests/tag_kill_measurement.rs`: 300 groups over 2
    # hosts compiles to 300 subscriptions PER HOST against a ceiling of 20,
    # while every filter carries 1 value out of a 500-value budget. Nothing
    # about the derived binding is the problem -- the same collapse is
    # missing for literal values, above.
    Given I administer 300 groups
    When I open the group state of every group I administer
    Then relay "hub" serves every "d" watch with 1 subscription
    And no subscription on relay "hub" carries more than 500 "d" values
    And every "d" value I watch is covered by some subscription on relay "hub"

  @wip
  Scenario: Learning about one more group replaces the subscription in place
    # A newly resolved value must widen the live subscription, not open
    # another one. Measured today: six groups, six subscriptions, no
    # replacement of any kind. Engine-level ledgers for the same behavior are
    # in `crates/nmp-engine/tests/core_headless/derived_tag_fanout.rs`.
    Given I administer 5 groups
    And the group state of every group I administer is open
    When I am made an admin of one more group
    Then relay "hub" serves every "d" watch with 1 subscription
    And relay "hub" widened that subscription in place
    And every "d" value I watch is covered by some subscription on relay "hub"

  @wip
  Scenario: Nothing already asked for is asked for again
    # Found by this harness, not previously reported, and NOT specific to the
    # tag axis: recompiling re-sends REQs whose subscription id AND filter
    # are byte-identical to what is already live. Measured: two tag watches
    # cost four REQs (two of them redundant); three author watches cost one
    # redundant REQ even on the axis that otherwise behaves. Each one makes
    # the relay re-run the query and re-stream everything it matches, so it
    # is wire cost on top of the fan-out rather than a consequence of it.
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
    # instead in `crates/nmp-engine/tests/core_headless/derived_tag_fanout.rs`,
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
