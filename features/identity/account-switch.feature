Feature: Switching accounts is clean
  @must-never @ledger-10
  Scenario: The old account's feed cannot leak into the new one
    Given only 1 indexer relay is configured
    And Carol's relay list names "carol-relay" as her write relay
    And Dave's relay list names "dave-relay" as his write relay
    And Carol has posted a note saying "hello from carol"
    And Dave has posted a note saying "hello from dave"
    And I am logged in as an account that follows Carol
    And my feed of my follows' notes is open
    Then my feed shows Carol's notes
    When I switch to a new account that follows Dave
    Then my feed shows Dave's notes
    And notes from Carol no longer arrive
