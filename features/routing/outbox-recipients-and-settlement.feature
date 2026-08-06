Feature: An outbox can finish -- unknown recipients keep it open, absent ones do not
  An outbox obligation is not open-ended. It ends, and Pablo's worked example
  of it ending is the specification for this whole file:

  > an outbox can end too; for example, if the user is p-tagging 3 users and only one of them has a 10002 and we know the other two don't have one, once we have the relays we'll publish to for the author's own relay + any app relay + some of the 1-p-tagged-user that did have a 10002 then the outbox item is consumed.

  Read that sentence for what it quietly requires. Knowing that two people
  DON'T HAVE a relay list is a different state from not having looked yet, and
  if the two are indistinguishable then no route with a listless recipient ever
  finishes. Pablo's ruling on how absence becomes knowable:

  > and "do we have a 10002 for these three users" is very knowable: the moment we receive EOSE from the indexer relays we use we know, one way or another, whether we have a 10002 or not.

  How that settlement is derived, cached, and re-probed after a restart is the
  knowledge-and-settlement arm's subject, not this file's. What is asserted
  here is only what the OUTBOX RESOLVER does with the three answers it gets:
  use a known list, skip a settled-absent one without waiting, and keep the
  route alive for one it has not yet been told about. Completion here means
  settled RESOLUTION, never delivery:

  > yeah, once we know "this event goes in relay 1, 2 and 3" it's been routed; it might have not been published and it sits on the publishing queue, but it's been routed. Whether you consider that "done" it depends on your position; it's done in terms of routing.

  Background:
    Given I am logged in as my own account
    And my relay list names "author-write-1" and "author-write-2" as my write relays
    And app relays "app-indexer" are configured

  # ---- the worked example ------------------------------------------------

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::three_recipients_with_one_relay_list_between_them_retire_the_obligation
  # nmp:falsifier=Keep the obligation open for a recipient whose lookup settled without a relay list; the owner's three-p-tag example never retires.
  Scenario: Three recipients, one relay list between them, and the outbox is consumed
    # Pablo's example, transcribed step for step. Bob has a kind:10002; Carol
    # and Dave definitively do not, and the indexers finishing their stored
    # events is what makes "do not" a fact rather than a guess. The route is
    # the author's own relays, plus the app relay, plus the one inbox that
    # exists -- and there is nothing left to learn, so the obligation retires
    # with two of its three recipients contributing nothing at all.
    Given Bob's relay list names "bob-inbox" as his read relay
    And the indexers have finished their stored events without a relay list for Carol
    And the indexers have finished their stored events without a relay list for Dave
    When I publish a note saying "the three of you" that p-tags Bob, Carol, and Dave
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer", and "bob-inbox"
    And routing is complete

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_settled_absent_recipient_adds_no_relay_and_delays_nothing
  # nmp:falsifier=Fold a settled absence into ignorance; it declares a route need, the answer never completes and the two states stop being distinguishable.
  Scenario: A settled-absent recipient neither adds a relay nor delays anything
    # The unit version of the rule above. "Settled absent" is a resolved
    # input: it contributes nothing to the route AND blocks nothing, which is
    # what makes it categorically different from the scenario immediately
    # below. Both look like an empty relay set from a collapsed list; only one
    # of them is an answer.
    Given the indexers have finished their stored events without a relay list for Bob
    When I publish a note saying "nowhere to reach you" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", and "app-indexer"
    And routing is complete

  # ---- unknown keeps it live ---------------------------------------------

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unlooked_up_recipient_keeps_the_answer_open_while_known_relays_are_used_now
  # nmp:falsifier=Read an unlooked-up recipient as having no inbox; the answer completes on a cold cache and the note is silently under-routed on every first run.
  Scenario: A recipient nobody has looked up yet keeps the routing open
    # The distinction that earns the three-valued model. Carol has not been
    # looked up, so her inbox is unknown -- not empty. Treating that as "she
    # has none" would silently under-route, permanently, on exactly the cold
    # cache every first run has. The relays already known are used NOW rather
    # than held back; incompleteness delays the finish, not the delivery.
    Given Bob's relay list names "bob-inbox" as his read relay
    And the indexers have not yet finished their stored events for Carol
    When I publish a note saying "Bob now, Carol when we know" that p-tags Bob and Carol
    Then the note is routed to "author-write-1"
    And the note is routed to "bob-inbox"
    And the note is routed to "app-indexer"
    And routing is not complete
    And the publish has not failed

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_recipients_arriving_relay_list_completes_the_route_it_was_holding_open
  # nmp:evidence=rust:nmp::fresh_and_recovered_auto_writes_share_one_later_author_route
  # nmp:falsifier=Resolve the route once at acceptance instead of re-executing the strategy when the missing relay list lands; the late inbox never joins and the app would have to publish again.
  Scenario: When the unknown settles, the route finishes without the app asking again
    # The wake. The app published once and holds one receipt; it never
    # re-submits, never polls, and never learns that a lookup was outstanding.
    # Carol turns out to have an inbox after all, it joins the same route, and
    # the obligation retires.
    Given Bob's relay list names "bob-inbox" as his read relay
    And the indexers have not yet finished their stored events for Carol
    And I published a note saying "Bob now, Carol when we know" that p-tags Bob and Carol
    When Carol's relay list arrives naming "carol-inbox" as her read relay
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer", "bob-inbox", and "carol-inbox"
    And routing is complete

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unknown_that_settles_absent_finishes_the_route_without_adding_a_relay
  # nmp:falsifier=Retire an obligation only when a settlement names a relay; every note to someone without a relay list stays routed forever.
  Scenario: An unknown that settles as absent finishes the route just as well
    # The other settlement outcome, and the one that makes retirement
    # reachable at all. Nothing new is added to the route; what changes is
    # that there is no longer anything to wait for. If only the first outcome
    # were implemented, every note to someone without a relay list would be
    # routed forever.
    Given Bob's relay list names "bob-inbox" as his read relay
    And the indexers have not yet finished their stored events for Carol
    And I published a note saying "Bob now, Carol when we know" that p-tags Bob and Carol
    When the indexers finish their stored events without a relay list for Carol
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer", and "bob-inbox"
    And routing is complete

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::one_unlooked_up_recipient_among_settled_ones_keeps_the_answer_open
  # nmp:falsifier=Complete the answer once most recipients have settled; the one unanswered addressee is dropped without anyone being told.
  Scenario: One unknown among many settled recipients is enough to keep it open
    # Completion is a property of the whole recipient set, not a majority of
    # it. Four of five are answered; the fifth alone holds the obligation
    # open, and the route it already has keeps being used meanwhile.
    Given Bob's relay list names "bob-inbox" as his read relay
    And Carol's relay list names "carol-inbox" as her read relay
    And the indexers have finished their stored events without a relay list for Dave
    And the indexers have finished their stored events without a relay list for Erin
    And the indexers have not yet finished their stored events for Frank
    When I publish a note saying "five of you" that p-tags Bob, Carol, Dave, Erin, and Frank
    Then the note is routed to "bob-inbox"
    And the note is routed to "carol-inbox"
    And routing is not complete

  # ---- the author's own list ---------------------------------------------

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_author_who_declared_no_write_relays_is_settled_not_unknown
  # nmp:falsifier=Read a present relay list whose write half is empty as not-yet-known; a published "I write nowhere in particular" parks the write on an answer that already arrived.
  Scenario: An author whose relay list declares zero write relays is known, not unknown
    # A published kind:10002 that names no write relays is an ANSWER. The
    # author said, on the record, "I write nowhere in particular" -- and the
    # route completes on the app relays alone rather than parking forever
    # waiting for a list that has already arrived. The three-valued author
    # fact (`AuthorRouteState::Present` with an empty outbound half, distinct
    # from `Unknown`) is what makes the distinction expressible at all; the
    # defect this rules out is collapsing a present-but-empty list into
    # ignorance on the way to the resolver.
    Given my relay list declares no write relays
    When I publish a note saying "I write nowhere in particular"
    Then the note is routed to exactly "app-indexer"
    And routing is complete

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-008
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_author_whose_entries_are_all_inbound_has_a_settled_empty_outbox
  # nmp:falsifier=Fall back to the author's inbound entries when their outbound half is empty; an all-read-marked list routes their own note to the relays they collect mail at.
  Scenario: A relay list that is all read-marked is the same known-empty case
    # The shape this actually turns up as in the wild: someone's list has
    # entries, all of them read-marked, so their WRITE set is empty while
    # their list plainly exists. Same conclusion -- known, complete, no park.
    Given my relay list names only "author-read-only" as a read-marked relay
    When I publish a note saying "all my entries are inbound"
    Then the note is routed to exactly "app-indexer"
    And the note is never routed to "author-read-only"
    And routing is complete

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-009
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unlooked_up_author_parks_the_route_and_keeps_the_operator_relay_it_has
  # nmp:evidence=rust:nmp::fresh_and_recovered_auto_writes_share_one_later_author_route
  # nmp:falsifier=Fail the resolution when the directory knows no write relays for the author yet; the first publish of a fresh install dies permanently on a signed, journalled event.
  Scenario: Publishing before my own relay list has ever been fetched waits, and does not die
    # The cold start, and the defect it rules out: a resolution that could
    # ERROR when the directory knew no write relays, composed with a routing
    # error removing the pending write, killed anything published before the
    # first relay-list fetch PERMANENTLY -- an event that is signed,
    # journalled and durable, lost because the directory was young.
    # `resolve_routes` is total instead: not knowing yet is the normal initial
    # state of this resolver, so it waits.
    #
    # This scenario asserts the ROUTE's shape only; how the park is reported
    # and replayed on the receipt belongs to the resolution-lifecycle
    # scenarios.
    Given the indexers have not yet finished their stored events for my own account
    When I publish a note saying "first run, cold cache"
    Then the note is routed to "app-indexer"
    And routing is not complete
    And the publish has not failed

  # nmp:id=ROUTING-OUTBOXSETTLEMENT-010
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_authors_arriving_relay_list_completes_the_route_that_waited_on_it
  # nmp:evidence=rust:nmp::fresh_and_recovered_auto_writes_share_one_later_author_route
  # nmp:falsifier=Journal the resolved relay set instead of the Auto strategy; a relay list that arrives after acceptance never reaches the parked write.
  Scenario: My own relay list arriving finishes a route that was waiting on it
    # The author arm of the wake. Same one receipt, same event, no
    # re-submission by the app.
    Given the indexers have not yet finished their stored events for my own account
    And I published a note saying "first run, cold cache"
    When my relay list arrives naming "author-write-1" and "author-write-2" as my write relays
    Then the note is routed to exactly "author-write-1", "author-write-2", and "app-indexer"
    And routing is complete
