Feature: One subscription per relay, not one per thing you asked about
  A query that resolves to many values -- every group I administer, every
  channel in a catalog -- must reach a relay as a handful of subscriptions
  carrying large value arrays, never as one subscription per value. Relays
  cap concurrent subscriptions at around 20; they accept arrays of 500
  values without complaint. Fanning out inverts that.

  Every scenario below is @wip: the behavior is the INTENDED behavior, and
  none of it holds today. Measured evidence for each gap lives in
  `crates/nmp-router/tests/tag_kill_measurement.rs`,
  `crates/nmp-router/tests/tag_fanout_churn.rs`, and
  `crates/nmp-engine/tests/core_headless/derived_tag_fanout.rs`. The step
  catalog has no vocabulary for groups, tag values, or subscription counts
  yet, so these scenarios also need new steps before they can run.

  @wip
  Scenario: A catalog of groups is one subscription, not three hundred
    # Measured today: 300 groups over 2 hosts compiles to 300 subscriptions
    # per host against a limit of 20, while every filter carries 1 value out
    # of a 500-value budget. The coalescing registry is a no-op on this axis.
    Given I administer 300 groups
    When I open the group state of every group I administer
    Then the host serves it with at most 2 subscriptions
    And no subscription carries more than 500 group identifiers
    And every group I administer is covered by some subscription

  @wip
  Scenario: Learning about one more group replaces the subscription in place
    # Measured today: each newly resolved value OPENS a new subscription.
    # A naive widened filter instead closes and reopens on every change --
    # 15 wire messages for 8 values, against 8 for the fan-out. Neither is
    # right: the author axis already achieves 8 messages and 1 subscription,
    # and this axis must match it.
    Given I administer 5 groups
    And the group state of every group I administer is open
    When I am made an admin of one more group
    Then the host receives one replacing request
    And no subscription is closed
    And the new group is covered by the same subscription as the others

  @wip
  Scenario: Collapsing does not wait for the relay to finish
    # Measured today: growth before end-of-stored-events, after it, and on a
    # relay that never sends it are byte-identical -- resolution is driven by
    # ingested rows alone. That must survive the collapse: a slow or silent
    # relay must not delay coverage of a group already known from elsewhere.
    Given I administer 3 groups
    And relay "slow-relay" never confirms end of stored events
    When I open the group state of every group I administer
    Then all 3 groups are covered without waiting for "slow-relay"

  @wip
  Scenario: A catalog already on disk opens as one subscription immediately
    # Measured today: a warm cache resolves the whole set in a single
    # recompile -- but emits one subscription per value. The single recompile
    # is right; the fan-out inside it is not.
    Given I administer 40 groups
    And the group state of every group I administer is already stored locally
    When I open the group state of every group I administer
    Then the host receives exactly one subscription
    And that subscription carries all 40 group identifiers

  @wip
  Scenario: Past the array bound the request splits rather than truncating
    # The bound is a frame-size limit, not a demand limit. Mirrors the
    # existing id-array bound, which chunks and ships the remainder as
    # further requests rather than silently dropping ids.
    Given I administer 1200 groups
    When I open the group state of every group I administer
    Then the host serves it with 3 subscriptions
    And every group I administer is covered by some subscription
    And no group is silently dropped

  @wip
  Scenario: Demands that differ in more than the collapsed values stay apart
    # This is the injectivity requirement. Two requests that cannot be merged
    # must not collapse onto one subscription id -- today a colliding pair is
    # silently reduced to one request that never repairs. Falsifier:
    # `limited_identical_except_authors_atoms_collide_on_sub_id`.
    Given two open queries that ask the same host for different things
    And the two queries cannot be served by one widened request
    When both are compiled for the wire
    Then the host receives two distinct subscriptions
    And neither query's demand is dropped

  @wip
  Scenario: Asking about two different tags never merges into one request
    # Within one tag name a value list is a choice; across tag names the
    # filter demands both. Merging "#d is x" with "#t is y" into a single
    # request would demand both tags and stop matching what either asked for.
    Given one open query for group state tagged "x"
    And one open query for events labelled "y"
    When both are compiled for the wire
    Then the host receives two distinct subscriptions
    And neither subscription demands both tags at once

  @wip
  Scenario: Asking about nothing asks for nothing
    # The privacy floor, and the one property that already holds. An empty
    # resolved set must never widen into an unfiltered request -- see
    # `probe_single_inner_value_reaches_the_wire_as_one_req`.
    Given I administer no groups
    When I open the group state of every group I administer
    Then no group state subscription is opened
    And the host is never asked for unfiltered group state
