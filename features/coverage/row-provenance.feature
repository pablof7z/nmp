# Per-relay read provenance is complete end to end -- and, until this file,
# guarded by nothing. `Provenance { seen, local }` projects to the app as
# `Row { event, sources }` with a `RowDelta::SourcesGrew` when a second relay
# delivers an event already held, and it reaches Swift and Kotlin undegraded.
# Every scenario below describes behaviour that exists TODAY, which is why
# none of them is `@designed`: they run.
Feature: A row says which relays served it

  Scenario: One event from two relays is one row naming both
    Given only 1 indexer relay is configured
    And Alice's relay list names "alice-north" and "alice-south" as her write relays
    And Alice has posted a note saying "same note, two relays"
    And I am logged in as my own account
    When I read Alice's notes from relays "alice-north" and "alice-south"
    Then my feed shows the note saying "same note, two relays"
    And the row saying "same note, two relays" names relays "alice-north" and "alice-south" as its sources

  Scenario: Two events from two relays stay attributable to each
    Given only 1 indexer relay is configured
    And Alice's relay list names "alice-relay" as her write relay
    And Bob's relay list names "bob-relay" as his write relay
    And Alice has posted a note saying "from Alice"
    And Bob has posted a note saying "from Bob"
    And I am logged in as an account that follows Alice and Bob
    When I open a feed of my follows' notes
    Then my feed shows the note saying "from Alice"
    And my feed shows the note saying "from Bob"
    And the row saying "from Alice" names relay "alice-relay" as its only source
    And the row saying "from Bob" names relay "bob-relay" as its only source

  # NIP-29 group metadata is signed by each HOST relay, so one group id
  # legitimately exists on two relays with two different signers and two
  # different contents. That is divergence, not a conflict: NMP surfaces both
  # versions, each attributable to the host that served it, and lets the app
  # navigate the disagreement rather than picking a winner. The addressable
  # coordinate includes the author pubkey, which is what makes this hold.
  Scenario: Two hosts disagreeing about one group both survive, distinctly
    Given only 1 indexer relay is configured
    And relay "host-north" hosts group "photographers" with metadata saying "Photographers of the North"
    And relay "host-south" hosts group "photographers" with metadata saying "Photographers of the South"
    And I am logged in as my own account
    When I read the metadata for group "photographers" from relays "host-north" and "host-south"
    Then my feed holds exactly 2 rows
    And the row saying "Photographers of the North" names relay "host-north" as its only source
    And the row saying "Photographers of the South" names relay "host-south" as its only source

  # Both relays are taken away BEFORE the restart, so nothing on the far side
  # can re-derive what the store already knew. Any source set the reconstructed
  # engine reports came off the journal.
  Scenario: Provenance survives a restart
    Given only 1 indexer relay is configured
    And Alice's relay list names "alice-north" and "alice-south" as her write relays
    And Alice has posted a note saying "durable provenance"
    And I am logged in as my own account
    When I read Alice's notes from relays "alice-north" and "alice-south"
    Then the row saying "durable provenance" names relays "alice-north" and "alice-south" as its sources
    When relay "alice-north" drops the connection
    And relay "alice-south" drops the connection
    And I reconstruct the engine from the same durable store
    And I read Alice's notes from relays "alice-north" and "alice-south"
    Then the row saying "durable provenance" names relays "alice-north" and "alice-south" as its sources
