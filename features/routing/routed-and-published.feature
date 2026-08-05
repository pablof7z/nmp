Feature: "We don't know where this goes" is not "we know, and it hasn't gone yet"
  Routed and published are separate axes, and an app must be able to tell three
  states apart at any moment: still determining destinations, destinations
  determined but not yet delivered, and delivered. Collapsing the first two
  into one "pending" produces a spinner that cannot say whether it is waiting
  on knowledge or on a socket -- which is the difference between a
  misconfigured indexer set and a slow relay.

  The receipt carries both axes without conflating them: whether routing is
  complete, which destinations are known so far, and then the ordinary
  per-relay delivery facts that already ship today.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/resolution-lifecycle.md` §7.2-7.3).

  Background:
    Given I am logged in as my own account
    And an indexer relay is configured

  Scenario: Determining destinations, with nothing known yet
    # The first state. Nothing to show but the fact that the question is open,
    # and the reason it is open.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports it is still determining destinations
    And the receipt reports no destinations yet
    And the receipt says why it is still determining destinations

  @designed
  Scenario: Determining destinations, with some already known
    # The same state, partially answered. An app can show "sending to 2 so far"
    # while being honest that the list may still grow -- both facts on one
    # receipt, neither implying the other.
    Given my relay list names "outbox-a" and "outbox-b" as my write relays
    And Dave's relay list has never been fetched
    When I publish a note saying "hello Dave" mentioning Dave
    Then the receipt reports "outbox-a" and "outbox-b" as destinations
    And the receipt reports it is still determining destinations

  @designed
  Scenario: Destinations known, nothing delivered yet
    # The second state, and the one that has no expression at all today. Every
    # destination is settled and none has been reached; "sending 0 of 2" is
    # only sayable if completeness and delivery are read separately.
    Given my relay list names "outbox-a" and "outbox-b" as my write relays
    And relay "outbox-a" cannot connect
    And relay "outbox-b" cannot connect
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports routing complete
    And the receipt reports "outbox-a" and "outbox-b" as destinations
    And the note is not delivered anywhere

  @designed
  Scenario: Some delivered while routing is still open
    # The two axes moving independently, in the direction people find
    # surprising: delivery has begun and progressed while the destination list
    # is still growing. Neither axis gates the other.
    Given my relay list names "outbox-a" as my write relay
    And Dave's relay list has never been fetched
    When I publish a note saying "hello Dave" mentioning Dave
    Then the receipt reports the note acked by "outbox-a"
    And the receipt reports it is still determining destinations
    When Dave's relay list arrives naming "dave-inbox" as his read relay
    Then the receipt reports routing complete
    And the receipt reports the note acked by "dave-inbox"

  @designed
  Scenario: Delivered everywhere it was ever going
    # The third state, and the only one that means "sent". It requires both
    # axes: routing complete, and every destination terminal.
    Given my relay list names "outbox-a" and "outbox-b" as my write relays
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports routing complete
    And the receipt reports the note acked by "outbox-a"
    And the receipt reports the note acked by "outbox-b"
    And every destination the note has is terminal

  @designed
  Scenario: Routed instantly and undeliverable forever reads differently from unroutable
    # The two stalls an app must never confuse. One write knows exactly where
    # it is going and cannot get there; the other does not know where it is
    # going. Same spinner, opposite remedies -- fix the relay, or fix the
    # discovery configuration.
    Given my relay list has never been fetched
    And no indexer relays are configured
    When I publish a note saying "unroutable" and let NMP figure out the routing
    And I publish a note saying "undeliverable" to exactly "wss://non-existent.com"
    Then the receipt for "unroutable" reports it is still determining destinations
    And the receipt for "undeliverable" reports routing complete
    And the receipt for "undeliverable" reports the failure to reach "wss://non-existent.com"
    And the two receipts report different stalls
