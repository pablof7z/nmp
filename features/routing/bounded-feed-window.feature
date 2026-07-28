Feature: A bounded window belongs to the feed, never to each author in it
  When an app asks for "the latest 20 notes from the people I follow", it is
  asking for 20 notes. Not 20 from each of them. A feed of one author is not a
  different mode -- it is this same rule where the feed happens to contain one
  author. The window is a property of the QUERY; the authors are a property of
  what the query selects.

  That sounds obvious and the wire disagreed with it. Routing splits an
  author-bearing demand into one atom per (author, relay) so each route can
  carry its own provenance, and the demand's `limit` rode along on every one
  of those atoms. An unbounded feed did not show the damage, because
  `coalesce::StructuralUnion` re-joined the shards downstream. A bounded one
  could not be re-joined at all -- `neither_limited` refuses any filter
  carrying a `limit` -- so it reached the wire as one REQ per author. Measured
  against real relays over a 1055-author follow list: ~351 wanted
  subscriptions against nos.lol's advertised cap of 20, for a page the user
  sees 20 of.

  The per-author split exists to carry PROVENANCE, which is router-side
  bookkeeping. It was never a reason to send one REQ per author, so the join
  now happens in routing and the merge registry is left alone.

  This does NOT weaken the rule that two independent limited watches stay
  apart (`neither_limited`, and the scenarios pinning it in
  subscription-collapse.feature). Two watches asking for 10 notes each
  genuinely want 10 each, and a merged `limit:10` would return 10 TOTAL. The
  per-author atoms of ONE feed were never two demands -- they are one demand
  that routing fanned, and re-joining them restores what was asked for rather
  than widening it.

  Every scenario here reads the REQ frames NMP actually put on the relay's
  socket, never the engine's own account of what it did.

  Background:
    Given I am logged in as my own account
    And relay "hub" is the relay I watch directly

  # ---- the subscription count -------------------------------------------

  Scenario: A bounded feed over many authors is one subscription, not one per author
    # The headline. Before this rule the wire carried one `limit:20` request
    # per author -- each promising 20 rows, together promising far more than
    # the page anyone asked for.
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "hub" as her write relay
    And Bob's relay list names "hub" as his write relay
    And Carol's relay list names "hub" as her write relay
    And Dave's relay list names "hub" as his write relay
    And Erin's relay list names "hub" as her write relay
    And I am logged in as an account that follows Alice, Bob, Carol, Dave, and Erin
    When I open a feed of the latest 20 of my follows' notes
    Then relay "hub" serves every author watch with 1 subscription

  Scenario: The one subscription carries the feed's window, not a per-author one
    # The limit must survive the join intact. Dropping it would substitute the
    # relay's own undocumented default and make under-fetch unobservable;
    # multiplying it by the author count would fetch a page-per-author again
    # under a different name.
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "hub" as her write relay
    And Bob's relay list names "hub" as his write relay
    And Carol's relay list names "hub" as her write relay
    And I am logged in as an account that follows Alice, Bob, and Carol
    When I open a feed of the latest 20 of my follows' notes
    Then every request on relay "hub" asks for at most 20 notes

  Scenario: A single-author feed is the same rule, not a special case
    # n=1. There is no per-author mode for this to be an instance of.
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "hub" as her write relay
    And I am logged in as an account that follows Alice
    When I open a feed of the latest 20 of my follows' notes
    Then relay "hub" serves every author watch with 1 subscription
    And every request on relay "hub" asks for at most 20 notes

  Scenario: Authors on different relays get one subscription each, not one per author
    # The join is per (relay, shape). Two relays means two requests, each
    # carrying the authors THAT relay was solved for -- never two per author.
    Given my relay list names "me-relay" as my write relay
    And Alice's relay list names "hub" as her write relay
    And Bob's relay list names "hub" as his write relay
    And Carol's relay list names "second-hub" as her write relay
    And Dave's relay list names "second-hub" as his write relay
    And I am logged in as an account that follows Alice, Bob, Carol, and Dave
    When I open a feed of the latest 20 of my follows' notes
    Then relay "hub" serves every author watch with 1 subscription
    And relay "second-hub" serves every author watch with 1 subscription

  # Scale is pinned at the router level instead of here:
  # `crates/nmp-router/tests/kill_measurement.rs` compiles a 300-author follow
  # list over a realistic overlapping write-relay distribution and asserts the
  # plan carries fewer subscriptions than there are authors EVEN WITH
  # COALESCING DISABLED -- which is the property this feature describes,
  # measured where 300 identities are cheap.

  # ---- what this must NOT change ----------------------------------------

  Scenario: Two independent bounded watches still stay apart
    # The regression guard, and the reason this was fixed in routing rather
    # than by relaxing `neither_limited`. These are two demands, not one
    # demand fanned: each genuinely wants its own 10 rows, and a merged
    # `limit:10` would return 10 total.
    When I watch for the latest 10 notes from Bob
    And I watch for the latest 10 notes from Carol
    Then relay "hub" serves every author watch with 2 subscriptions

  Scenario: A bounded watch and an unbounded one for the same author stay apart
    # Folding the bounded one into the unbounded one drops its bound; folding
    # the other way truncates the unbounded one. Neither is a widening.
    When I watch for the latest 10 notes from Bob
    And I watch for notes from Bob
    Then relay "hub" serves every author watch with 2 subscriptions
