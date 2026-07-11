Feature: The engine never talks to a relay it has no reason to
  @must-never @ledger-3 @ledger-4
  Scenario: Every connection traces to a routing decision
    Given only 2 indexer relays are configured
    And Alice's relay list names "relay-a" as her write relay
    And Bob's relay list names "relay-b" as his write relay
    And a relay "bystander" exists that nothing references
    And Alice has posted a note saying "hello from alice"
    And Bob has posted a note saying "hello from bob"
    And I am logged in as an account that follows Alice and Bob
    When my feed of my follows' notes runs to a steady state
    Then every contacted relay appears in the diagnostics with its routing lane
    And no relay outside the indexers, "relay-a", and "relay-b" was ever contacted
    And relay "bystander" received no connection at all
