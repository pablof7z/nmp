Feature: Publishing tells the truth, per relay
  # nmp:id=WRITES-RECEIPTS-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::write_ack_per_relay_over_real_relays
  # nmp:falsifier=Report one aggregate verdict for a two-relay publish; an app cannot tell which destination broke, and a note that reached one relay of two is shown as sent.
  @ledger-9
  Scenario: One note, two relays, two different answers
    Given my relay list names "good-relay" and "flaky-relay" as my write relays
    And relay "flaky-relay" rejects every event
    And I am logged in as my own account
    When I publish a note saying "hello"
    Then publishing returned a receipt id without waiting for anything
    And the receipt reports the note acked by "good-relay"
    And the receipt reports the note rejected by "flaky-relay"

  # nmp:id=WRITES-RECEIPTS-002
  # nmp:status=specified
  # nmp:gap=evidence
  # nmp:issue=#1253
  @ledger-9 @ledger-15 @wip
  Scenario: Durable acceptance survives restart through the ordinary store
    Given an unsigned kind 9999 draft matches an open ordinary query
    When the durable write is accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    Then the ordinary query shows the same pending row
    And the receipt can be reattached by its stable id

  # nmp:id=WRITES-RECEIPTS-003
  # nmp:status=specified
  # nmp:gap=evidence
  # nmp:issue=#1253
  @ledger-10 @ledger-19 @wip
  Scenario: An offline remote signer leaves a durable obligation
    Given a NIP-46 signer is registered for the current pubkey but is offline
    When I publish an unsigned kind 9999 draft
    Then the canonical pending row is visible to matching queries
    And the receipt reports awaiting that pubkey's signer
    When the matching signer provider reattaches
    Then the same row is promoted to signed after exact validation

  # nmp:id=WRITES-RECEIPTS-004
  # nmp:status=specified
  # nmp:gap=evidence
  # nmp:issue=#1253
  @ledger-15 @wip
  Scenario: Relay rejection does not retract a signed row
    Given a signed kind 9999 row is visible in a matching query
    When every planned relay rejects its publication
    Then the receipt records each relay rejection
    And the signed row remains visible
