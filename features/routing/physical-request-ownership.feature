Feature: Running relay requests and later local ownership

  Relay request filters are immutable once sent. A later observation may reuse
  running work only when that work fully covers its requested events; mere
  compatibility with a filter that could have been grouped before either
  request was sent is not coverage.

  Rule: A later observation executes every event range not already running

    Scenario: Compatibility with a running request does not suppress later work
      Given one immutable request is running for Alice's profile
      And a later independent observation asks for Bob's profile from the same relay
      When the later observation reaches relay admission
      Then NMP sends a request for Bob's profile
      And the request for Alice remains byte-for-byte unchanged

    # Executable known-violation artifact: rust:nmp-router::representable_running_filter_residual_is_executed_and_owned_as_one_lifecycle
    Scenario: A representable uncovered residual executes as one later lifecycle
      Given a running request covers kinds 0 and 1 for Alice and Bob
      And a later observation asks for kind 1 for Alice, Bob, and Carol
      When the later observation reaches relay admission
      Then NMP sends only the residual kind 1 request for Carol
      And the later observation owns both the incumbent and residual requests
      And closing the original observation keeps both requests running
      And closing the later observation closes both requests

    # Executable known-violation artifact: rust:nmp::split_request_pieces_commit_wide_coverage_only_after_every_piece_finishes
    Scenario: Split physical coverage becomes fresh only after every piece finishes
      Given a later observation is covered by one incumbent request piece and one residual piece
      When only the incumbent piece reaches end of stored events
      Then the complete later observation is not marked fresh
      When only the residual piece reaches end of stored events
      Then the complete later observation is not marked fresh by that piece alone
      When both pieces have eligible stored-event completions
      Then NMP persists the complete later coverage at the intersection of their proven intervals

  Rule: Byte-changing successors use fresh physical identities

    Scenario: Byte-changing replacement opens before retiring its predecessor
      Given one accepted immutable request is current on a relay session
      When changed wire bytes require a successor request
      Then the successor uses a fresh never-reused subscription id
      And NMP offers the successor before closing the predecessor
      And local refusal keeps the predecessor live and owns exactly one retry
      When the exact successor handoff is accepted
      Then NMP retires the predecessor and late predecessor EOSE proves nothing
      And repeated accepted replacements retain only one current request and one bounded transition
