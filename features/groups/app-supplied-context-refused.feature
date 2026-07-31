Feature: The h and previous tags belong to the group, not to the caller
  An app hands the group an event; it does not hand the group its own opinion
  about which group that event is in, or where it sits in the group's timeline.
  Both are refused with a typed error, and both are refused before signing, so
  a rejected publication leaves no signature and no journal row behind.

  Traces to docs/internals/nip29/group-publication.md sections 5, 8 and 9 (the
  surviving no-previous rule).

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::caller_supplied_own_h_is_refused_before_signing_or_routing
  # nmp:falsifier=skipping the CallerSuppliedContext check for a draft already carrying this group's own h makes caller_supplied_own_h_is_refused_before_signing_or_routing observe Ok instead of the typed refusal
  @nip29
  Scenario: An event already carrying this group's own h is still refused
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the error names the h tag
    And relay "wss://relay.groups.example" received no event
    And the signer was never asked to sign
    And no write intent was accepted

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-002
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::caller_supplied_other_group_h_is_refused_the_same_way
  # nmp:falsifier=making the refusal conditional on the caller's h value matching this group makes caller_supplied_other_group_h_is_refused_the_same_way observe Ok for a non-matching value
  @nip29
  Scenario: An event carrying another group's h is refused the same way
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "darkroom"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the refusal is the same error as for a matching h
    And relay "wss://relay.groups.example" received no event

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_caller_supplied_previous_is_refused
  # nmp:falsifier=skipping the CallerSuppliedTimeline check for a caller-supplied previous tag makes a_caller_supplied_previous_is_refused observe Ok instead of the typed refusal
  @nip29
  Scenario: An event carrying a previous tag is refused
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries a previous tag
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-previous error
    And the error names the previous tag
    And relay "wss://relay.groups.example" received no event
    And the signer was never asked to sign

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-004
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::combined_h_and_previous_is_refused_deterministically_on_whichever_tag_came_first
  # nmp:falsifier=checking h before previous regardless of the caller's own tag order makes the previous-first case in combined_h_and_previous_is_refused_deterministically_on_whichever_tag_came_first return CallerSuppliedContext instead of CallerSuppliedTimeline
  @nip29
  Scenario: An event carrying both is refused on the first one, not silently trimmed
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    And that event carries a previous tag
    When I publish that event through the group
    Then the publication is refused with a typed error
    And neither tag was stripped from the event I supplied
    And relay "wss://relay.groups.example" received no event

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-005
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::the_unsigned_door_never_invents_a_previous_tag
  # nmp:falsifier=appending a previous row from nmp_nip29::contextualize makes the_unsigned_door_never_invents_a_previous_tag observe a previous row on the minted draft
  @nip29
  Scenario: The unsigned group-publication door never invents a previous tag
    When I publish an event of kind 9 with content "first light" through the group
    Then the delivered event carries no previous tag
    And the unsigned group-publication door never invents or accepts a caller-supplied previous tag

  # nmp:id=PROTOCOL-APPSUPPLIEDCONTEXTREFUSED-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_caller_supplied_context_is_refused_before_relay_contact_and_differs_from_a_relay_rejection
  # nmp:falsifier=letting a caller-supplied-context draft reach the relay before the local refusal makes host.contact_count() nonzero in a_caller_supplied_context_is_refused_before_relay_contact_and_differs_from_a_relay_rejection
  @nip29
  Scenario: A refused publication is distinguishable from a rejected one
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the refusal is reported as a caller error, not as a relay rejection
    And no receipt was created for it
