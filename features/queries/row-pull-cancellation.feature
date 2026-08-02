Feature: A cancelled row pull loses no transition and repeats none
  An unbounded row observation is a stream of transitions: every frame an app
  applies is an exact step from the frame it applied last. So a pull that is
  cancelled must leave the observation exactly where it was, and a pull the app
  actually received must never come back. Apps get cancelled constantly --
  timeouts, screens closing, the process being killed -- and every one of those
  moments is a chance to silently drift away from what the engine knows.

  Rule: A transition counts as delivered only once the app really has it

    # nmp:id=QUERIES-ROWPULL-001
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::ready_frame_is_retained_until_foreign_commit_and_replayed_after_abort
    # nmp:evidence=kotlin:NMPKotlin::generatedReadyThenCancellationFreesFutureAndAbortsTicketBeforeCompletion
    # nmp:falsifier=Treat a transition as delivered the moment it is produced rather than when the app acknowledges it; the retry must then return a different frame from the one that was lost.
    Scenario: A pull cancelled at the instant its row was produced keeps that row
      Given an app is pulling rows from a live observation
      And the engine has produced the next transition
      When the app is cancelled before it can take that transition
      Then the next pull returns exactly the same transition
      And applying it leaves the app at the state the engine expects

    # nmp:id=QUERIES-ROWPULL-002
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::ready_frame_is_retained_until_foreign_commit_and_replayed_after_abort
    # nmp:evidence=rust:nmp-ffi::commit_refusals_are_typed_and_leave_the_candidate_unchanged
    # nmp:falsifier=Keep the transition after the app acknowledges it; the following pull must then repeat a row the app already applied.
    Scenario: A transition the app received is never handed out again
      Given an app has pulled and acknowledged a transition
      When the app pulls again
      Then it receives the next transition, never the one it already applied

    # nmp:id=QUERIES-ROWPULL-003
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::dropping_a_ticket_without_settling_rolls_the_delta_back_like_abort
    # nmp:falsifier=Roll back only on an explicit cancellation and not when the pull is simply released; a pull abandoned without cleanup must then lose its transition.
    Scenario: An app killed mid-pull loses nothing it never saw
      Given an app has started a pull and the engine has produced a transition
      When the app disappears without cancelling, acknowledging, or cleaning up
      Then the transition is still waiting for the next pull
      And the observation accepts a new pull immediately

    # nmp:id=QUERIES-ROWPULL-004
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::abandoning_a_parked_ticket_loses_no_row_produced_afterwards
    # nmp:falsifier=Leave the abandoned pull owning the observation; the next pull must then be refused instead of delivering the later row.
    Scenario: Abandoning an idle pull leaves the observation able to deliver later rows
      Given an app is waiting on a pull and nothing has changed yet
      When the app abandons that pull
      And a row is created afterwards
      Then the next pull delivers that row

    # nmp:id=QUERIES-ROWPULL-005
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::aborting_a_waiting_ticket_retains_the_delta_that_arrives_meanwhile
    # nmp:falsifier=Discard the transition that lands during the cancellation, or hand it to the cancelled caller anyway; the row is then either lost or delivered to a caller that has already given up.
    Scenario: A row that arrives while a pull is being cancelled belongs to the next pull
      Given an app is waiting on a pull
      When cancellation and the arrival of a row happen at the same moment
      Then the cancelled pull is told it was cancelled rather than handed the row
      And the next pull delivers that row

  Rule: Cancelling repeatedly costs nothing

    # nmp:id=QUERIES-ROWPULL-006
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::repeated_ready_cancellation_keeps_one_claim_and_one_composed_successor
    # nmp:falsifier=Hold one pending transition per cancellation instead of folding later changes together; memory must then grow with the number of cancellations and the replay must stop being the oldest unseen transition.
    Scenario: A hundred cancel-and-retry cycles hold one transition, not a queue
      Given an app repeatedly starts a pull and cancels it while rows keep changing
      When the app finally lets a pull finish
      Then it receives the one transition it never saw
      And everything that changed in between arrives as a single combined follow-up
      And the app ends up at exactly the engine's current set of rows

  Rule: One pull at a time, and never a shared copy

    # nmp:id=QUERIES-ROWPULL-007
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::ready_frame_is_retained_until_foreign_commit_and_replayed_after_abort
    # nmp:evidence=rust:nmp-ffi::concurrent_next_on_one_handle_is_a_typed_error
    # nmp:falsifier=Release ownership as soon as the transition is produced rather than when the app acknowledges it; a second pull can then start in that window and both callers receive the same transition.
    Scenario: A second pull on the same observation is refused, even mid-handover
      Given an app is already pulling from an observation
      When something starts a second pull on that same observation
      Then the second pull is refused with a named error
      And it never receives a copy of the first pull's transition

    # nmp:id=QUERIES-ROWPULL-008
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::one_ticket_admits_exactly_one_receive_whatever_the_first_is_doing
    # nmp:falsifier=Let a pull start a second delivery instead of refusing it; one pull can then hand out two rows, or two callers can share one.
    Scenario: One pull yields at most one row
      Given an app has started a pull
      When anything asks that same pull for a row a second time
      Then it is refused with a named error rather than served a second row
      And it makes no difference whether the first row had arrived yet

  Rule: Finishing a pull the wrong way is refused, and changes nothing

    # nmp:id=QUERIES-ROWPULL-009
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::commit_refusals_are_typed_and_leave_the_candidate_unchanged
    # nmp:falsifier=Let an early acknowledgement discard the pending transition anyway; the pull must then still deliver its row afterwards.
    Scenario: Acknowledging a row that has not arrived is refused and destroys nothing
      Given an app has started a pull that has not produced a row yet
      When the app acknowledges it anyway
      Then it is refused with a named error
      And the pull still delivers its row normally afterwards

    # nmp:id=QUERIES-ROWPULL-010
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::commit_refusals_are_typed_and_leave_the_candidate_unchanged
    # nmp:falsifier=Let a finished pull's acknowledgement apply to whichever pull is current; a stale acknowledgement then silently consumes a later app's transition.
    Scenario: A finished pull can never reach into a later one
      Given an app cancelled a pull and started a new one
      When the old pull is acknowledged late
      Then it is refused with a named error
      And the new pull's transition is untouched

    # nmp:id=QUERIES-ROWPULL-011
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::commit_abort_cancel_race_never_resurrects_or_duplicates_a_delta
    # nmp:falsifier=Let two of the three outcomes both take effect; a transition is then either delivered twice or resurrected after the observation closed.
    Scenario: Acknowledging, cancelling, and closing at once produce exactly one outcome
      Given an app acknowledges a pull, cancels it, and closes the observation simultaneously
      When all three land together
      Then exactly one of them takes effect and the others report what happened
      And no transition is delivered twice, stranded, or brought back after closing

  Rule: Closing the observation is the end of it

    # nmp:id=QUERIES-ROWPULL-012
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::cancel_wakes_a_parked_next_to_none_and_is_idempotent
    # nmp:evidence=rust:nmp-ffi::commit_abort_cancel_race_never_resurrects_or_duplicates_a_delta
    # nmp:falsifier=Keep a held transition across the close; a pull started afterwards then receives a row from an observation the app already ended.
    Scenario: Closing an observation ends a waiting pull and replays nothing afterwards
      Given an app is waiting on a pull
      When the app closes the observation
      Then the waiting pull ends with end-of-stream rather than hanging
      And closing it a second time changes nothing
      And no later pull can bring back a transition the closed observation held

    # nmp:id=QUERIES-ROWPULL-013
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::shutdown_wakes_all_pending_next_to_none
    # nmp:falsifier=Leave a waiting pull unwoken when the engine stops; the app then hangs forever on a dead engine.
    Scenario: Stopping the engine ends every waiting pull
      Given many observations each have a pull waiting for a row
      When the engine is stopped
      Then every waiting pull promptly reports end-of-stream

    # nmp:id=QUERIES-ROWPULL-014
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::a_retained_delta_survives_engine_shutdown_and_precedes_the_terminal_result
    # nmp:falsifier=Discard a held transition when the engine stops, the way closing the observation does; the app then loses a change the engine had already produced.
    Scenario: Stopping the engine does not eat a transition it already produced
      Given a pull was cancelled and its transition is being held
      When the engine is stopped without the app closing the observation
      Then the next pull still delivers that held transition
      And only then does the app see end-of-stream

  Rule: A windowed view is a picture, not a step

    # nmp:id=QUERIES-ROWPULL-015
    # nmp:status=built
    # nmp:evidence=rust:nmp-ffi::abort_does_not_replay_a_self_contained_window_snapshot
    # nmp:falsifier=Hold and replay an abandoned windowed view the way an unbounded transition is held; the app then receives a stale picture instead of the current one.
    Scenario: An abandoned windowed view is not replayed, because it was never a step
      Given an app is reading a windowed query, which delivers whole current views
      When the app abandons a pull that had produced a view
      Then that view is not held for the next pull
      But the app has lost nothing it needed, because the next view it receives is complete on its own

  Rule: The handshake stays out of the app's way

    # nmp:id=QUERIES-ROWPULL-016
    # nmp:status=built
    # nmp:evidence=swift:NMP::testCancellingAfterAcknowledgementWithdrawsTheWholeObservationBeforeDelivery
    # nmp:evidence=kotlin:NMPKotlin::cancellingAfterCommitButBeforeEmitWithdrawsTheWholeObservation
    # nmp:falsifier=Let cancellation that lands after acknowledgement return without withdrawing the observation; a later pull then continues from a transition the app never applied.
    Scenario: Cancelling after a row is acknowledged ends the observation rather than skipping the row
      Given the app's toolkit has acknowledged a transition on the app's behalf
      When the app is cancelled before that transition reaches it
      Then the whole observation is withdrawn
      And no later pull continues from a transition the app never applied

    # nmp:id=QUERIES-ROWPULL-017
    # nmp:status=built
    # nmp:evidence=swift:NMP::testRowTicketCommitsBeforeSwiftMapsTheFrame
    # nmp:evidence=kotlin:NMPKotlin::generatedReadyThenCancellationFreesFutureAndAbortsTicketBeforeCompletion
    # nmp:falsifier=Acknowledge after transforming or emitting the row instead of before; cancellation can then land between receiving and acknowledging, and the row is delivered twice.
    Scenario: The row is acknowledged before anything else can interrupt
      Given a platform SDK has just received a transition
      When it prepares that transition for the app
      Then it acknowledges the transition first, before any step that could be cancelled

    # nmp:id=QUERIES-ROWPULL-018
    # nmp:status=built
    # nmp:evidence=parity:nmp-parity::direct_and_ffi_facades_are_semantically_identical_over_real_loopback
    # nmp:falsifier=Skip, duplicate, or reorder a transition in the two-phase handover; the cross-boundary reader must then disagree with the in-process reader about the rows.
    Scenario: An app across the platform boundary sees exactly what an in-process reader sees
      Given the same query is read in-process and through a platform SDK against one relay
      When both read to completion
      Then they end up with identical rows and identical evidence for them
