Feature: A settled absence lives only as long as the session that derived it
  "We looked and found nothing" is an observation about this session's sources,
  not durable protocol data. It is never written to the store, so a restart
  forgets it and looks again -- the one fact that is expected to change is the
  one fact never cached across processes. A relay list, which has an author and
  a signed timestamp, survives the restart untouched; that asymmetry is the
  point.

  Background:
    Given only 2 indexer relays are configured
    And I am logged in as my own account
    And my relay list names "wss://mine.example" as my write relay

  @designed
  Scenario: A restart forgets a settled absence and probes again
    Given both indexers confirmed end of stored events having sent no relay list for Carol
    And the engine knows Carol has no relay list
    When I reconstruct the engine from the same durable store
    Then the engine does not know whether Carol has a relay list
    And no absence for Carol was read back from the store
    When I publish a note mentioning Carol
    Then routing for the note is not complete
    And the indexers are asked for Carol's relay list again
    When both indexers confirm end of stored events having sent no relay list for Carol
    Then routing for the note is complete

  @designed
  Scenario: A write recovered from the journal re-parks until discovery settles again
    Given I published a note mentioning Carol and the process stopped while it was parked
    When I reconstruct the engine from the same durable store
    Then the note is still awaiting routing
    And the note is not reported as failed
    And routing for the note is not resumed from a remembered absence
    And the indexers are asked for Carol's relay list again
    When both indexers confirm end of stored events having sent no relay list for Carol
    Then routing for the note is complete

  @designed
  Scenario: The relay list survives the restart, the absence does not
    Given Bob's relay list naming "wss://bob-relay.example" was ingested and stored
    And both indexers confirmed end of stored events having sent no relay list for Carol
    When I reconstruct the engine from the same durable store
    And I publish a note mentioning Bob and Carol
    Then the note is published to "wss://bob-relay.example" with no new lookup for Bob
    And routing for the note is not complete until Carol's absence is settled again
