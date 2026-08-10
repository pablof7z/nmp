Feature: Replaceable writes keep only useful delivery work
  @ledger-9

  # nmp:id=WRITES-REPLACEABLE-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-store::every_nip01_replaceable_class_retires_its_offline_predecessor
  # nmp:evidence=rust:nmp::superseding_a_replaceable_write_cancels_its_pending_signer
  # nmp:evidence=rust:nmp::subscribe_publish_and_reconnect_replay_over_a_real_relay
  # nmp:falsifier=disabling same-coordinate retirement leaves both obligations recoverable or lets the older signer and relay work continue
  Scenario Outline: A newer offline replaceable write retires the older obligation
    Given my relay list names "offline-relay" as my write relay
    And I am logged in as my own account
    When relay "offline-relay" drops the connection
    And I publish kind <kind> with d tag "<d>" saying "older"
    Then the first receipt reports waiting for "offline-relay"
    When I publish kind <kind> with d tag "<d>" saying "newer"
    Then the first receipt reports superseded by the newer replaceable write
    And the second receipt reports waiting for "offline-relay"
    When relay "offline-relay" comes back
    Then the second receipt reports acked by "offline-relay"

    Examples:
      | kind  | d        |
      | 0     | ignored  |
      | 3     | ignored  |
      | 10001 | ignored  |
      | 30001 | presence |

  Rule: Obsolete local attempts are not durable history

    # A receipt is safety evidence for work that may have crossed a handoff.
    # It is not an archive of bytes NMP proved it never sent. Replaceable state
    # makes that distinction exact: after a newer value wins, an older unsent
    # value can never become useful again.

    # nmp:id=WRITES-REPLACEABLE-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-store::superseded_unsent_body_and_receipt_are_destroyed_across_redb_reopen
    # nmp:evidence=rust:nmp-store::explicit_not_handed_off_evidence_destroys_the_obsolete_receipt_and_correlation
    # nmp:evidence=rust:nmp::cancellation_never_restores_an_unpublished_replaceable_predecessor
    # nmp:falsifier=removing exact supersession retirement leaves the old event or receipt visible after Redb reopens
    Scenario: A newer replaceable write destroys its unsent predecessor
      Given an unpublished kind 0 write for my account
      When I publish a newer kind 0 write for the same account
      Then only the newer write remains in my publish queue
      And the older event body and delivery work no longer exist after restart
      And cancelling the newer write does not restore the older unpublished value
      And a started attempt explicitly proven not handed off is treated as unsent

    # nmp:id=WRITES-REPLACEABLE-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::expired_local_acceptance_is_refused_before_custody_and_retains_nothing
    # nmp:evidence=rust:nmp::already_expired_publish_is_refused_before_receipt_custody
    # nmp:falsifier=routing AlreadyExpired through retained refusal custody allocates a receipt and fails the empty queue assertion
    Scenario: An already-expired attempt never becomes durable write history
      Given a presence event whose expiration has already passed
      When I try to publish it
      Then publishing is refused before NMP takes custody
      And no event, receipt, signer request, relay lane, or attempt is retained

    # Some replacement evidence cannot disappear immediately: bytes may have
    # crossed a transport handoff, and forgetting that uncertainty would invite
    # an unsafe blind retry. That narrow safety record is still finite history,
    # not permission to accumulate every prior value forever.

    # nmp:id=WRITES-REPLACEABLE-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-store::a_newer_replaceable_stops_an_older_started_obligation_but_keeps_bounded_safety_evidence
    # nmp:evidence=rust:nmp-store::superseded_safety_receipts_are_bounded_by_age_and_count
    # nmp:evidence=rust:nmp-store::superseded_safety_receipt_deadline_survives_redb_reopen
    # nmp:evidence=rust:nmp::superseded_safety_receipt_is_pruned_by_the_engine_deadline
    # nmp:evidence=rust:nmp-parity::direct_and_ffi_reattach_are_semantically_identical_for_a_terminal_retained_receipt
    # nmp:falsifier=removing either the age deadline or count eviction leaves more than 500 superseded receipts or preserves one past an hour
    Scenario: Safety evidence for replaced writes is strictly bounded
      Given replaceable writes whose older values may have crossed a handoff
      Then their old event bodies and delivery machinery are permanently removed
      And only a terminal safety receipt represents the possible handoff
      And its terminal says the write was superseded after a possible handoff, not never sent
      When their retained safety evidence becomes one hour old
      Then NMP permanently removes that obsolete evidence
      And if more than 500 replaced entries accumulate sooner, NMP keeps only the newest 500
      But current obligations are never classified as disposable
      And possible-handoff ambiguity is retained only as that bounded safety evidence

    # nmp:id=WRITES-REPLACEABLE-005
    # nmp:status=built
    # nmp:evidence=rust:nmp-store::replaceable_retirement_refuses_a_truncated_not_handed_off_attempt_record
    # nmp:falsifier=trusting only the valid NotHandedOff prefix deletes the predecessor receipt and correlation despite the malformed required tail
    Scenario: Corrupt delivery evidence is never mistaken for safe deletion
      Given a persisted attempt says it was not handed off
      But the rest of that attempt record is incomplete or malformed
      When a newer replaceable write would retire the old attempt
      Then NMP refuses the replacement transaction as corrupt state
      And the old receipt and correlation remain untouched
      And no partial deletion is committed
