Feature: Publishing tells the truth, per relay
  @ledger-9
  Scenario: One note, two relays, two different answers
    Given my relay list names "good-relay" and "flaky-relay" as my write relays
    And relay "flaky-relay" rejects every event
    And I am logged in as my own account
    When I publish a note saying "hello"
    Then publishing returned a receipt id without waiting for anything
    And the receipt reports the note acked by "good-relay"
    And the receipt reports the note rejected by "flaky-relay"

  @ledger-9 @ledger-15
  Scenario: Durable acceptance survives restart through the ordinary store
    Given an unsigned kind 9999 draft matches an open ordinary query
    When the durable write is accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    Then the ordinary query shows the same final event id and body as pending
    And its closed signature value carries no signature bytes
    And the receipt can be reattached by its stable id

  @ledger-10 @ledger-19
  Scenario: A delayed signer promotes the row an open query already received
    Given an ordinary query is open for an unsigned kind 9999 draft
    And the matching signer has not answered
    When I publish an unsigned kind 9999 draft
    Then the query receives the canonical row as pending
    And the receipt reports awaiting that pubkey's signer
    When a signer for a different pubkey attaches
    Then the same row remains pending with no update
    When the matching signer answers with an exactly valid signature
    Then the query updates that same event id to signed
    And the updated row's Signed arm carries the exact verified signature

  @ledger-15
  Scenario: A verified relay event is signed rather than locally pending
    Given a relay sends a valid signed event matching an ordinary query
    When NMP verifies and stores that event
    Then the query reports the row as signed
    And the row carries the relay's verified signature

  @ledger-15
  Scenario: Relay rejection does not retract a signed row
    Given a signed kind 9999 row is visible in a matching query
    When every planned relay rejects its publication
    Then the receipt records each relay rejection
    And the signed row remains visible

  @ledger-9
  Scenario: An app awaits one terminal publication answer
    Given NMP accepted a write and returned its receipt
    When the app awaits the receipt result
    Then NMP returns exactly one typed whole-write outcome
    And the app implements no receipt-state reducer

  @ledger-9
  Scenario: Relay disagreement survives terminal reduction
    Given one destination published the event
    And another destination rejected it with a reason
    When the receipt result completes
    Then both final relay states are present
    And the rejection reason remains attached to the rejecting relay

  @ledger-9 @ledger-10
  Scenario: Signer refusal is a terminal publication result
    Given NMP accepted an unsigned write
    When its signer refuses and compensation commits
    Then the receipt reports that signing was refused
    And the receipt ends as not sent because the signer refused

  @ledger-9 @ledger-15
  Scenario: Terminal result recovers from live receipt lag
    Given live receipt delivery exceeded its bounded memory window
    And the complete receipt remains durable
    When the app awaits the receipt result
    Then NMP restarts from retained receipt history
    And returns the same terminal answer without exposing a replay cursor

  @ledger-9 @ledger-15
  Scenario: Restarted app retrieves the terminal result by receipt id
    Given an accepted receipt survived process restart
    When the app asks NMP for that receipt's result
    Then NMP traverses retained pages and returns its terminal answer
    And the app implements no cursor or replay loop

  Rule: NMP bounds completed receipt history without weakening retained evidence

    Scenario: Retained terminal receipts keep their complete history until whole eviction
      Given completed receipts with different outcomes and detailed relay attempts
      When their internal terminal-history boundary has not been reached
      Then reattachment returns the same complete facts as before termination
      And the app sees no compacted summary or retention policy
      When the oldest terminal receipt crosses the internal boundary
      Then NMP removes that receipt and all of its exclusively-owned evidence together
      And no still-open receipt is removed

    Scenario: All terminal outcomes share one internal oldest-first history
      Given completed writes include acknowledgements, refusals, cancellations, no destinations, and superseded attempts
      When completed history reaches its internal boundary
      Then the oldest completed receipt is removed regardless of its outcome
      And no app configures a per-kind or per-coordinate retention policy

  Rule: A storage failure costs progress, never an accepted write

    @ledger-9 @ledger-15
    Scenario: A storage failure refuses that operation and nothing else
      Given an app is using one persistent Engine
      When one durable operation fails
      Then that operation reports the failure and no acceptance
      And the next write is accepted through the same Engine
      And the app performs no recovery orchestration
