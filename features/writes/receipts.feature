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
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unsigned_write_is_still_explicitly_pending_after_a_restart
  # nmp:evidence=rust:nmp::pending_row_and_frozen_signer_resume_after_reopen_then_cancel_compensates
  # nmp:evidence=rust:nmp::pending_has_no_signature_or_event_projection
  # nmp:evidence=rust:nmp-ffi::pending_ffi_row_contains_no_signature_sentinel
  # nmp:falsifier=Expose the store's sentinel as an app signature, or infer Signed from the store's Event shape after reopen; the cold query either leaks fake bytes or loses Pending and the same durable obligation.
  @ledger-9 @ledger-15
  Scenario: Durable acceptance survives restart through the ordinary store
    Given an unsigned kind 9999 draft matches an open ordinary query
    When the durable write is accepted and the process stops immediately
    And I reconstruct the engine from the same durable store
    Then the ordinary query shows the same final event id and body as pending
    And its closed signature value carries no signature bytes
    And the receipt can be reattached by its stable id

  # nmp:id=WRITES-RECEIPTS-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::delayed_signer_promotes_the_same_visible_row_from_pending_to_signed
  # nmp:evidence=rust:nmp::signer_unavailable_keeps_accepted_row_visible
  # nmp:evidence=rust:nmp::slow_observer_never_retains_a_pending_row_after_signature_promotion
  # nmp:evidence=rust:nmp::signed_always_projects_the_exact_supplied_signature
  # nmp:evidence=swift:NMP::testRowAccumulatorSignaturePromotionReplacesTheSameRow
  # nmp:evidence=kotlin:NMPKotlin::signaturePromotionReplacesTheSameRow
  # nmp:falsifier=Split signature bytes from their lifecycle state, or omit the closed signature value from remembered-row comparison or mailbox composition; an invalid combination becomes constructible or an open or slow observation retains Pending while a native accumulator may append a duplicate.
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

  # nmp:id=WRITES-RECEIPTS-010
  # nmp:status=built
  # nmp:evidence=rust:nmp::ordinary_room_batch_queries_only_the_matching_handle_and_skips_router_compile
  # nmp:falsifier=Classify every row without local provenance as pending; a relay-verified event reaches the ordinary query with the wrong signature state.
  @ledger-15
  Scenario: A verified relay event is signed rather than locally pending
    Given a relay sends a valid signed event matching an ordinary query
    When NMP verifies and stores that event
    Then the query reports the row as signed
    And the row carries the relay's verified signature

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

  Rule: NMP bounds completed receipt history without weakening retained evidence

    # nmp:id=WRITES-RECEIPTS-011
    # nmp:status=built
    # nmp:evidence=rust:nmp-store::retained_terminal_receipt_keeps_full_history_until_whole_eviction
    # nmp:evidence=rust:nmp-store::terminal_retention_whole_closure_eviction_is_atomic_across_process_death
    # nmp:falsifier=Delete only the terminal receipt row; its correlation or per-relay attempt facts survive as an orphan after reopen.
    Scenario: Retained terminal receipts keep their complete history until whole eviction
      Given completed receipts with different outcomes and detailed relay attempts
      When their internal terminal-history boundary has not been reached
      Then reattachment returns the same complete facts as before termination
      And the app sees no compacted summary or retention policy
      When the oldest terminal receipt crosses the internal boundary
      Then NMP removes that receipt and all of its exclusively-owned evidence together
      And no still-open receipt is removed

    # nmp:id=WRITES-RECEIPTS-012
    # nmp:status=built
    # nmp:evidence=rust:nmp-store::all_terminal_receipt_kinds_share_one_fifo
    # nmp:evidence=rust:nmp-store::terminal_age_count_and_bytes_each_force_whole_eviction
    # nmp:evidence=rust:nmp-store::terminal_receipt_fifo_survives_redb_reopen
    # nmp:evidence=rust:nmp-store::retained_terminal_receipt_keeps_full_history_until_whole_eviction
    # nmp:evidence=rust:nmp-store::a_newer_replaceable_stops_an_older_started_obligation_but_keeps_bounded_safety_evidence
    # nmp:falsifier=Order terminal retention by receipt allocation rather than completion; two writes completing in reverse order evict the wrong receipt.
    Scenario: All terminal outcomes share one internal oldest-first history
      Given completed writes include acknowledgements, refusals, cancellations, no destinations, and superseded attempts
      When completed history reaches its internal boundary
      Then the oldest completed receipt is removed regardless of its outcome
      And no app configures a per-kind or per-coordinate retention policy

  Rule: A persistent Engine owns recovery from a transient storage failure

    # nmp:id=WRITES-STORE-RECOVERY-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::persistent_engine_recovers_latched_store_and_resolves_ambiguous_acceptance_once
    # nmp:evidence=rust:nmp-store::reopen_replaces_only_the_database_generation_and_preserves_durable_identity
    # nmp:falsifier=Disable the runtime recovery driver; after an acceptance I/O closes the real Redb generation, the same Engine never reconstructs the committed row or accepts a later write.
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
    # nmp:falsifier=Expose the interrupted pre-signed acceptance as Signed, create a second receipt on exact retry, or let divergent or invalid bytes promote it; the recovered row lies or durable identity splits.
    @ledger-9 @ledger-15
    Scenario: An uncertain acceptance is resolved from durable identity
      Given a write has an app-owned correlation token
      And its acceptance transaction reports an uncertain storage failure
      When the same Engine reconstructs its durable state
      Then NMP reads the correlation token back before repeating acceptance
      And the committed body remains pending without signature bytes
      When the exact valid signed event is retried with that correlation token
      Then that same receipt and row are promoted to signed
      And divergent or invalid retry bytes cannot promote the row
      And exactly one durable receipt owns the write

    # nmp:id=WRITES-STORE-RECOVERY-003
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1644
    @ledger-9
    Scenario: A store that cannot be reconstructed never fabricates acceptance
      Given the durable store remains unavailable
      When the app publishes through the existing Engine
      Then publishing does not report acceptance
      And NMP keeps retrying internal recovery with bounded backoff
