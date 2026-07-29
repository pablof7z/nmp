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

  Every scenario here is @designed.

  Background:
    Given I am logged in as my own account
    And my relay list names "author-write-1" and "author-write-2" as my write relays
    And app relays "app-indexer" are configured

  # ---- the worked example ------------------------------------------------

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

  Scenario: An author whose relay list declares zero write relays is known, not unknown
    # A published kind:10002 that names no write relays is an ANSWER. The
    # author said, on the record, "I write nowhere in particular" -- and the
    # route completes on the app relays alone rather than parking forever
    # waiting for a list that has already arrived. Master gets the underlying
    # fact right (`knows_write_relays` is documented as "known, possibly zero"
    # and flips on ingest, `crates/nmp-router/src/facts.rs:119-140`) and then
    # throws it away, because the outbox arm sees only a collapsed empty `Vec`
    # and errors on it.
    Given my relay list declares no write relays
    When I publish a note saying "I write nowhere in particular"
    Then the note is routed to exactly "app-indexer"
    And routing is complete

  Scenario: A relay list that is all read-marked is the same known-empty case
    # The shape this actually turns up as in the wild: someone's list has
    # entries, all of them read-marked, so their WRITE set is empty while
    # their list plainly exists. Same conclusion -- known, complete, no park.
    Given my relay list names only "author-read-only" as a read-marked relay
    When I publish a note saying "all my entries are inbound"
    Then the note is routed to exactly "app-indexer"
    And the note is never routed to "author-read-only"
    And routing is complete

  Scenario: Publishing before my own relay list has ever been fetched waits, and does not die
    # The cold start, and the master defect it is written against: a routing
    # error at `on_signed` removes the pending write and emits
    # `WriteStatus::Failed` (`crates/nmp/src/core/write.rs:2229-2243`), while
    # the outbox arm errors whenever the directory knows no write relays
    # (`:2599-2600`). Compose the two and publishing anything before the first
    # relay-list fetch dies PERMANENTLY -- an event that is signed, journaled
    # and durable, killed because the directory was young. Not knowing yet is
    # the normal initial state of this resolver, so it waits.
    #
    # This scenario asserts the ROUTE's shape only; how the park is reported
    # and replayed on the receipt belongs to the resolution-lifecycle
    # scenarios.
    Given the indexers have not yet finished their stored events for my own account
    When I publish a note saying "first run, cold cache"
    Then the note is routed to "app-indexer"
    And routing is not complete
    And the publish has not failed

  Scenario: My own relay list arriving finishes a route that was waiting on it
    # The author arm of the wake. Same one receipt, same event, no
    # re-submission by the app.
    Given the indexers have not yet finished their stored events for my own account
    And I published a note saying "first run, cold cache"
    When my relay list arrives naming "author-write-1" and "author-write-2" as my write relays
    Then the note is routed to exactly "author-write-1", "author-write-2", and "app-indexer"
    And routing is complete
