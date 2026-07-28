Feature: With nowhere to ask, nothing settles and writes park forever
  Settlement needs sources that finish. An engine configured with no indexer
  relays has none, so no absence is ever derived and every write with a missing
  author parks indefinitely. That is the intended behaviour, not a gap: the
  alternative is treating "nowhere to ask" as "asked, nothing there", which
  silently under-routes every write on a misconfigured app. The park is loud
  and carries its reason; the guess does not exist.

  Background:
    Given no indexer relays are configured
    And I am logged in as my own account
    And my relay list names "wss://mine.example" as my write relay

  @designed
  Scenario: A missing author parks the write indefinitely rather than guessing
    Given no relay list for Carol has ever been ingested
    When I publish a note mentioning Carol
    Then the note is published to "wss://mine.example"
    And routing for the note is not complete
    And no relays are guessed for Carol
    And the engine never claims Carol has no relay list
    And the note is still parked awaiting Carol's relay list after every drain
    And the note is not reported as failed

  @designed
  Scenario: The stall is visible with its reason
    Given no relay list for Carol has ever been ingested
    When I publish a note mentioning Carol
    Then diagnostics report the note as stalled determining its destinations
    And the reason names Carol's relay list as the missing knowledge

  @designed
  Scenario: Content relays are not used as a discovery fallback
    Given no relay list for Carol has ever been ingested
    And relay "wss://mine.example" would answer a relay-list request
    When I publish a note mentioning Carol
    Then no relay-list request is sent to "wss://mine.example"
    And routing for the note is not complete

  @designed
  Scenario: Only writes with missing knowledge park
    Given Bob's relay list names "wss://bob-relay.example" as his read relay
    When I publish a note mentioning Bob
    Then the note is published to "wss://mine.example" and "wss://bob-relay.example"
    And routing for the note is complete

  @designed
  Scenario: Configuring an indexer unblocks the parked write
    Given no relay list for Carol has ever been ingested
    And I published a note mentioning Carol and it is parked
    When an indexer relay is configured
    Then the indexers are asked for Carol's relay list
    When the indexer confirms end of stored events having sent no relay list for Carol
    Then routing for the note is complete
    And the note was never reported as failed while it was parked
