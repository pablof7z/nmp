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
  @ledger-9 @ledger-15
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
  @ledger-10 @ledger-19
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
  @ledger-15
  Scenario: Relay rejection does not retract a signed row
    Given a signed kind 9999 row is visible in a matching query
    When every planned relay rejects its publication
    Then the receipt records each relay rejection
    And the signed row remains visible

  # nmp:id=WRITES-RECEIPTS-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::receipt_result_returns_one_terminal_answer_without_app_reduction
  # nmp:falsifier=Require every app to drain and interpret WriteFact itself; two apps can give different answers for the same durable receipt.
  @ledger-9
  Scenario: An app awaits one terminal publication answer
    Given NMP accepted a write and returned its receipt
    When the app awaits the receipt result
    Then NMP returns exactly one typed whole-write outcome
    And the app implements no receipt-state reducer

  # nmp:id=WRITES-RECEIPTS-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::mixed_relay_result_preserves_each_terminal_truth
  # nmp:falsifier=Collapse a mixed publish and rejection to one boolean; the app loses which relay rejected and why.
  @ledger-9
  Scenario: Relay disagreement survives terminal reduction
    Given one destination published the event
    And another destination rejected it with a reason
    When the receipt result completes
    Then both final relay states are present
    And the rejection reason remains attached to the rejecting relay

  # nmp:id=WRITES-RECEIPTS-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::terminal_signer_errors_compensate_the_write
  # nmp:falsifier=End an accepted write with only a signing-progress refusal; result waits forever because no whole-write terminal exists.
  @ledger-9 @ledger-10
  Scenario: Signer refusal is a terminal publication result
    Given NMP accepted an unsigned write
    When its signer refuses and compensation commits
    Then the receipt reports that signing was refused
    And the receipt ends as not sent because the signer refused

  # nmp:id=WRITES-RECEIPTS-008
  # nmp:status=built
  # nmp:evidence=rust:nmp-ffi::receipt_result_recovers_from_live_fifo_lag_without_exposing_replay
  # nmp:falsifier=Treat live-stream lag as terminal loss; an app awaiting the result fails even though durable replay contains the complete receipt.
  @ledger-9 @ledger-15
  Scenario: Terminal result recovers from live receipt lag
    Given live receipt delivery exceeded its bounded memory window
    And the complete receipt remains durable
    When the app awaits the receipt result
    Then NMP restarts from retained receipt history
    And returns the same terminal answer without exposing a replay cursor

  # nmp:id=WRITES-RECEIPTS-009
  # nmp:status=built
  # nmp:evidence=rust:nmp::restart_reattachment_returns_the_same_terminal_answer_without_cursor_code
  # nmp:falsifier=Make a restarted app traverse receipt pages and reduce them itself; restart requires a second app-specific receipt implementation.
  @ledger-9 @ledger-15
  Scenario: Restarted app retrieves the terminal result by receipt id
    Given an accepted receipt survived process restart
    When the app asks NMP for that receipt's result
    Then NMP traverses retained pages and returns its terminal answer
    And the app implements no cursor or replay loop

  Rule: A persistent Engine owns recovery from a transient storage failure

    # nmp:id=WRITES-STORE-RECOVERY-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once
    # nmp:evidence=rust:nmp-store::reopen_replaces_only_the_database_generation_and_preserves_durable_identity
    # nmp:falsifier=Disable the runtime recovery driver; the same Engine never accepts or reattaches a later write after the injected handle latch.
    @ledger-9 @ledger-15
    Scenario: A transient storage failure does not require a new Engine
      Given an app is using one persistent Engine
      And the durable store becomes temporarily unwritable
      When the store becomes writable again
      Then the same Engine reconstructs its durable state
      And a later write can be accepted without app recovery orchestration

    # nmp:id=WRITES-STORE-RECOVERY-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once
    # nmp:falsifier=Repeat the uncertain acceptance without correlation readback; the retained publish queue contains two receipts instead of one.
    @ledger-9 @ledger-15
    Scenario: An uncertain acceptance is resolved from durable identity
      Given a write has an app-owned correlation token
      And its acceptance transaction reports an uncertain storage failure
      When the same Engine reconstructs its durable state
      Then NMP reads the correlation token back before repeating acceptance
      And exactly one durable receipt owns the write

    # nmp:id=WRITES-STORE-RECOVERY-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once
    # nmp:evidence=rust:nmp::persistent_engine_does_not_reconstruct_for_an_invariant_fault
    # nmp:evidence=rust:nmp::recovery_backoff_is_exponential_event_driven_and_capped
    # nmp:falsifier=Treat either injected unavailable reopen as success; publishing can report acceptance before durable reconstruction completes.
    @ledger-9
    Scenario: A store that cannot be reconstructed never fabricates acceptance
      Given the durable store remains unavailable
      When the app publishes through the existing Engine
      Then publishing does not report acceptance
      And NMP keeps retrying internal recovery with bounded backoff
