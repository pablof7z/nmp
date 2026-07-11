Feature: Publishing tells the truth, per relay
  @ledger-9
  Scenario: One note, two relays, two different answers
    Given my relay list names "good-relay" and "flaky-relay" as my write relays
    And relay "flaky-relay" rejects every event
    And I am logged in as my own account
    When I publish a note saying "hello"
    Then the receipt first reports only accepted -- never sent
    And the receipt reports the note acked by "good-relay"
    And the receipt reports the note rejected by "flaky-relay"
