Feature: An Auto ends when there is nothing left to learn
  An `Auto` entry retires when its resolution has NO REMAINING UNKNOWNS --
  never when every relay has acked. Retirement is knowledge exhaustion;
  delivery is a separate business entirely.

  The owner's worked example is the whole rule: "if the user is p-tagging 3
  users and only one of them has a 10002 and we know the other two don't have
  one, once we have the relays we'll publish to for the author's own relay +
  any app relay + some of the 1-p-tagged-user that did have a 10002 then the
  outbox item is consumed."

  Note the clause that makes it reachable: "we KNOW the other two don't have
  one". Not "we haven't found one". Absence has to be positive knowledge, and
  it becomes positive knowledge at EOSE -- "the moment we receive EOSE from the
  indexer relays we use we know, one way or another, whether we have a 10002 or
  not". Where absence and ignorance are indistinguishable, nothing here retires
  at all.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/resolution-lifecycle.md` §7,
  `docs/internals/routing/knowledge-and-settlement.md`).

  Background:
    Given I am logged in as my own account
    And my relay list names "outbox-a" as my write relay
    And an indexer relay is configured

  # ---- the owner's case ------------------------------------------------

  @designed
  Scenario: Three mentioned users, one with a relay list and two settled as having none
    # Verbatim from the ruling, as a scenario. Dave contributes his inbox;
    # Erin and Frank contribute nothing -- and contributing nothing is a
    # RESOLVED input, not a pending one, because the indexer finished its
    # stored events without producing a relay list for either. Zero unknowns
    # remain, the answer can never change again, and the entry is consumed.
    Given Dave's relay list names "dave-inbox" as his read relay
    When I publish a note saying "hello all three" mentioning Dave, Erin, and Frank
    And the indexer finishes its stored events with no relay list for Erin or Frank
    Then the note is delivered to "outbox-a"
    And the note is delivered to "dave-inbox"
    And the receipt reports routing complete
    And the routing entry is consumed
    And the strategy is never executed again for that note

  @designed
  Scenario: One user still unsettled keeps the entry alive
    # The control for the scenario above, and the reason the distinction is
    # load-bearing. Same shape, except the indexer never confirms end of stored
    # events for Frank -- so Frank is UNKNOWN rather than known-absent, and the
    # entry cannot retire however much else is settled.
    Given Dave's relay list names "dave-inbox" as his read relay
    When I publish a note saying "hello all three" mentioning Dave, Erin, and Frank
    And the indexer finishes its stored events with no relay list for Erin
    And the indexer never confirms end of stored events for Frank
    Then the note is delivered to "dave-inbox"
    And the receipt reports it is still determining destinations
    And the routing entry is not consumed

  @designed
  Scenario: Every relay acked, one unknown left, and routing is still not complete
    # Retirement is not delivery, stated as its most counterintuitive
    # consequence: everything that has a destination has been delivered and
    # acked, and the entry stays live because something is still unknown.
    # Anything that flips completeness on ack has the axes crossed.
    Given Dave's relay list has never been fetched
    When I publish a note saying "hello Dave" mentioning Dave
    Then the receipt reports the note acked by "outbox-a"
    And every destination the note has is acked
    And the receipt reports it is still determining destinations

  # ---- routed is not published -----------------------------------------

  @designed
  Scenario: A fully resolved route whose delivery has not happened is routed
    # "once we know 'this event goes in relay 1, 2 and 3' it's been routed; it
    # might have not been published and it sits on the publishing queue, but
    # it's been routed. Whether you consider that 'done' it depends on your
    # position; it's done in terms of routing."
    Given relay "outbox-a" cannot connect
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports routing complete
    And the receipt reports "outbox-a" as a destination
    And the note is not delivered anywhere

  Scenario: An explicit route retires at its first resolution
    # The fixed point: verbatim execution has no inputs, therefore no unknowns,
    # ever. It is complete the instant it is resolved, before any relay has
    # been contacted.
    When I publish a note saying "hello" to exactly "chosen-relay"
    Then the receipt reports routing complete
    And the routing entry is consumed

  # ---- what cannot settle ----------------------------------------------

  @designed
  Scenario: With no indexers configured nothing settles, and the entry parks
    # Fail-closed, and correct: an engine with no discovery sources CANNOT
    # know, and treating "nowhere to ask" as "asked, nothing there" would
    # silently under-route every write. So the entry parks visibly and forever
    # rather than guessing, and the park carries its reason so a configuration
    # error reads as one.
    Given no indexer relays are configured
    And Dave's relay list has never been fetched
    When I publish a note saying "hello Dave" mentioning Dave
    Then the note is delivered to "outbox-a"
    And the receipt reports it is still determining destinations
    And the receipt says why it cannot settle
    And the write is still held, not dropped

  @designed
  Scenario: An author who publishes a relay list after being settled as absent
    # Absence is a cache of "nothing existed as of settlement", never a
    # tombstone. Erin settles absent, the note retires -- and if Erin publishes
    # a relay list an hour later, that later fact simply replaces the absence
    # for everything that comes after. The retired note is not reopened: its
    # answer was final for the p-tags frozen in the event it signed.
    When I publish a note saying "hello Erin" mentioning Erin
    And the indexer finishes its stored events with no relay list for Erin
    Then the receipt reports routing complete
    When Erin's relay list arrives naming "erin-inbox" as her read relay
    Then "erin-inbox" was never contacted for that note
    And the strategy is never executed again for that note
