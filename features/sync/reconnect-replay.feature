Feature: Reconnecting resumes with no gap
  Scenario: A dropped relay's subscriptions are replayed on reconnect, no resubscribe needed
    Given only 1 indexer relay is configured
    And Bob's relay list names "bob-relay" as his write relay
    And I am logged in as an account that follows Bob
    And my feed of my follows' notes is open
    When relay "bob-relay" drops the connection
    And relay "bob-relay" comes back
    And Bob posts a note saying "hello after reconnect"
    Then my feed shows the note saying "hello after reconnect"
