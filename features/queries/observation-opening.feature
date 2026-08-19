Feature: Observation opening is all-or-nothing
  An observation exists only after its initial canonical local view can be
  read. A failed attempt returns a typed refusal and cannot change another
  observation, contact a relay, or leave work alive without an app owner.

  Rule: Initial canonical projection failure creates no observation

    Scenario: An ordinary observation refuses an unreadable initial view
      Given an existing live observation is using the available relay capacity
      When an app opens another ordinary observation whose initial local view cannot be read
      Then opening returns ObservationUnavailable without an observation handle
      And the existing observation's rows, evidence, and relay plan stay unchanged
      And a later healthy empty query still opens with one honest initial frame

    Scenario: A windowed observation refuses an unreadable initial view
      Given an existing live window is using the available relay capacity
      When an app opens another window whose initial local view cannot be read
      Then opening returns ObservationUnavailable without a window handle
      And the existing window's rows, evidence, and relay plan stay unchanged
      And a later healthy empty window still opens with one honest initial frame

  Rule: Shutdown remains a different lifecycle fact

    Scenario: Shutdown and projection refusal are never conflated
      Given observation opening and engine shutdown overlap
      When the initial canonical view is unreadable before shutdown takes ownership
      Then the app receives ObservationUnavailable and no reply is dropped
      But an observation attempted after shutdown receives EngineClosed

  Rule: Resolver drops survive every opening refusal

    Scenario: An opening refusal still reports earlier dropped demand
      Given a resolver handle was dropped before another observation begins opening
      When the new observation refuses during graph construction or initial projection
      Then the resolver outcome carries the already-drained close delta
      And that close is consumed in the same reducer call
      And no later poll can report the same close again
