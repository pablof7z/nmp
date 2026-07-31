Feature: Observation opening is all-or-nothing
  An observation exists only after its initial canonical local view can be
  read. A failed attempt returns a typed refusal and cannot change another
  observation, contact a relay, or leave work alive without an app owner.

  Rule: Initial canonical projection failure creates no observation

    # nmp:id=QUERIES-OPENING-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::observation_open_failures_are_typed_leak_free_and_leave_runtime_usable
    # nmp:evidence=rust:nmp::ordinary_projection_refusal_cannot_perturb_a_cap_sized_existing_plan
    # nmp:falsifier=Recompile or register a receiver before the ordinary query's first canonical read; the ownership census or cap-sized sibling plan must change.
    Scenario: An ordinary observation refuses an unreadable initial view
      Given an existing live observation is using the available relay capacity
      When an app opens another ordinary observation whose initial local view cannot be read
      Then opening returns ObservationUnavailable without an observation handle
      And the existing observation's rows, evidence, and relay plan stay unchanged
      And a later healthy empty query still opens with one honest initial frame

    # nmp:id=QUERIES-OPENING-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::observation_open_failures_are_typed_leak_free_and_leave_runtime_usable
    # nmp:evidence=rust:nmp::history_projection_refusal_cannot_perturb_a_cap_sized_existing_window
    # nmp:falsifier=Keep the new window session after its first canonical read fails; the history-owner census or cap-sized sibling window must change.
    Scenario: A windowed observation refuses an unreadable initial view
      Given an existing live window is using the available relay capacity
      When an app opens another window whose initial local view cannot be read
      Then opening returns ObservationUnavailable without a window handle
      And the existing window's rows, evidence, and relay plan stay unchanged
      And a later healthy empty window still opens with one honest initial frame

  Rule: Shutdown remains a different lifecycle fact

    # nmp:id=QUERIES-OPENING-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::shutdown_queued_during_each_refusal_keeps_the_typed_reply_and_never_panics
    # nmp:evidence=rust:nmp::every_verb_fails_closed_after_shutdown
    # nmp:falsifier=Drop the opening reply when shutdown is queued or relabel a post-shutdown call as ObservationUnavailable; one of the lifecycle proofs must fail.
    Scenario: Shutdown and projection refusal are never conflated
      Given observation opening and engine shutdown overlap
      When the initial canonical view is unreadable before shutdown takes ownership
      Then the app receives ObservationUnavailable and no reply is dropped
      But an observation attempted after shutdown receives EngineClosed
