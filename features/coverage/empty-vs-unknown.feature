Feature: Unknown is not the same as empty
  @ledger-7
  Scenario: Emptiness is only claimed when it is proven
    Given only 1 indexer relay is configured
    And Alice's relay list names "alice-relay" as her write relay
    And relay "alice-relay" never confirms end of stored events
    And I am logged in as an account that follows Alice
    When I open a feed of my follows' notes
    Then my feed is empty
    And the query reports its results are unknown -- not empty
