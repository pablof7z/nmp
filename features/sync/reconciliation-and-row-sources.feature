# A row's source set answers "which relays in this read are known to hold this
# event", never "which relay happened to answer first". Reconciliation is where
# that distinction is easiest to lose: NIP-77 compares an id set against the
# relay it is talking to, so whatever NMP claims to already hold is what that
# relay will decline to send back. Claiming another relay's holdings buys a
# saved round trip and pays for it with a permanently wrong source set, because
# an id excluded from the comparison is never requested, never delivered, and
# therefore never attributed. Nothing revisits it afterwards.
#
# The cost of getting this right is deliberate: an event that two relays both
# hold is fetched from both. A source set is a claim about relays, and the only
# thing that substantiates it is that relay having served the event.
Feature: Which relays hold a row does not depend on which one answered first

  Scenario: A relay that also holds an already-delivered event still becomes one of its sources
    Given relays "north" and "south" both hold the same note
    And "north" has already delivered that note, so it is in the local cache
    When I read that note from relays "north" and "south"
    Then the row for that note names relays "north" and "south" as its sources

  # A read pinned to two relays must not let the second one collect sources it
  # never earned. This is the guard against "reconciliation lost a source" being
  # repaired by naming every relay the read was aimed at.
  Scenario: A relay in the read that does not hold the event is never named a source
    Given I am reading from relays "accepting-host" and "refusing-host"
    And I publish a note to both of them
    And "accepting-host" acknowledges the note while "refusing-host" refuses it
    When my feed shows the note
    Then the row for that note names relay "accepting-host" as its only source
    And the row appears exactly once

  # "In the cache" and "carried by relays" are two facts, and an empty source
  # set is the honest answer to the second one rather than an absence of an
  # answer. A row the app can already see with no relay behind it yet is what
  # every optimistic write looks like for its first moments.
  Scenario: A row held only in the cache names no relays at all
    Given none of the relays I am reading from are reachable
    When I publish a note and my feed shows it
    Then the row for that note names no relays as its sources

  # Source sets are stored, not re-derived from whoever answers after a restart.
  # A reopened store that has lost a source would recreate exactly the defect
  # above without any reconciliation being involved.
  Scenario: A row keeps its full source set across a restart
    Given a note has been delivered by relays "north" and "south"
    And both relays are unreachable
    When I reopen the same durable store and read that note
    Then the row for that note names relays "north" and "south" as its sources
