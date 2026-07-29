Feature: Offering the same signed event twice costs bandwidth, not correctness
  By the time anything reaches the wire the event is signed and its id is
  fixed, so a relay receiving it twice deduplicates it: "publishing to relay1
  event with id 1 twice is completely harmless". That is what makes re-running
  the strategy at every send opportunity safe at all.

  Harmless is not free, though -- "We don't want to go overboard so as to not
  waste bandwidth" -- so the design bounds the waste structurally rather than
  by building dedup machinery against a cost problem. Destinations are keyed by
  (intent, relay), and each resolution appends only what is new, so re-running
  resolution costs an empty diff. The residue is at most one redundant offer
  per destination per ambiguity window, which is the floor any at-least-once
  delivery system pays.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/resolution-lifecycle.md` §§6, 9).

  Background:
    Given I am logged in as my own account
    And an indexer relay is configured

  Scenario: An acked destination is never offered the event again
    # The core suppression. An acked destination is terminal and untouched by
    # any later resolution, however many times the strategy runs.
    Given my relay list names "outbox-a" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports the note acked by "outbox-a"
    When the engine ticks 20 times
    And the publishing queue drains 3 times with nothing new learned
    Then "outbox-a" was offered the note exactly once

  Scenario: A restart after delivery does not resend to relays that already acked
    # The same rule across a process boundary, which is where a design that
    # kept relay sets instead of strategies would resend everything.
    Given my relay list names "outbox-a" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports the note acked by "outbox-a"
    When the process stops
    And I reconstruct the engine from the same durable store
    Then "outbox-a" was offered the note exactly once

  @designed
  Scenario: A newly revealed destination is contacted and the settled ones are left alone
    # Diff-and-append, observed from the wire: learning Dave's inbox costs
    # exactly one new offer, and nothing already delivered is disturbed.
    Given my relay list names "outbox-a" as my write relay
    And Dave's relay list has never been fetched
    When I publish a note saying "hello Dave" mentioning Dave
    Then the receipt reports the note acked by "outbox-a"
    When Dave's relay list arrives naming "dave-inbox" as his read relay
    Then the note is delivered to "dave-inbox"
    And "outbox-a" was offered the note exactly once

  @designed
  Scenario: A relay list re-arriving unchanged costs nothing at all
    # Ingesting the same kind:10002 again is an ordinary event in a running
    # engine. Re-resolution against unchanged knowledge must produce an empty
    # diff, not a re-offer.
    Given my relay list names "outbox-a" and "outbox-b" as my write relays
    When I publish a note saying "hello" and let NMP figure out the routing
    And my relay list arrives again, unchanged
    Then "outbox-a" was offered the note exactly once
    And "outbox-b" was offered the note exactly once

  @designed
  Scenario: A duplicate offer after a crash mid-send is harmless
    # The one redundant offer the design accepts, and the reason it is
    # acceptable rather than a bug to engineer away: the process died between
    # putting the event on the wire and hearing the ack, so both "it arrived"
    # and "it didn't" are live. Re-offering a signed event id is the cheap,
    # correct move.
    Given my relay list names "outbox-a" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    And the process stops after the note reaches the wire but before the ack
    And I reconstruct the engine from the same durable store
    Then "outbox-a" is offered the note again
    And "outbox-a" holds one copy of the note
    And the receipt reports one delivery for "outbox-a"
    And the note is not duplicated anywhere else

  @designed
  Scenario: A relay that already holds the event answers the second offer, not a second write
    # Idempotency at the relay level, from the receipt's side: two offers of
    # one signed event id produce one delivered event and one delivery fact,
    # never two rows and never a confused receipt.
    Given my relay list names "outbox-a" as my write relay
    And relay "outbox-a" already holds the note being published
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports the note acked by "outbox-a"
    And "outbox-a" holds one copy of the note
    And exactly one receipt exists for that publish

  @designed
  Scenario: Two inputs resolving to the same relay make one destination, not two
    # The overlap case, and a reason destinations are keyed by (intent, relay)
    # rather than by whatever produced them. My own write relay and Dave's
    # inbox happen to be the same host; that is one obligation, offered once.
    Given my relay list names "outbox-a" as my write relay
    And Dave's relay list names "outbox-a" as his read relay
    When I publish a note saying "hello Dave" mentioning Dave
    Then the receipt reports exactly one destination
    And "outbox-a" was offered the note exactly once
