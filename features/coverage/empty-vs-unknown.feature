Feature: Empty local rows remain distinct from acquisition evidence
  @ledger-7
  Scenario: Emptiness is only claimed when it is proven
    Given only 1 indexer relay is configured
    And Alice's relay list names "alice-relay" as her write relay
    And relay "alice-relay" never confirms end of stored events
    And I am logged in as an account that follows Alice
    When I open a feed of my follows' notes
    Then my feed is empty
    And the query does not claim its empty result is complete

  @ledger-7 @ledger-18 @wip
  Scenario: Planned sources report independent facts
    Given a kind 9999 query plans relay "finished", relay "offline", and relay "private"
    And relay "finished" has finished its request with no matches
    And relay "offline" cannot connect
    And relay "private" requires AUTH
    When I observe the query snapshot
    Then its local rows are empty
    And its acquisition evidence reports all three source facts separately
    And it reports no global complete or authoritative-empty state
