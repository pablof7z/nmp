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

  @designed @nip29
  Scenario: An event already carrying this group's own h is still refused
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the error names the h tag
    And relay "wss://relay.groups.example" received no event
    And the signer was never asked to sign
    And no write intent was accepted

  @designed @nip29
  Scenario: An event carrying another group's h is refused the same way
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "darkroom"
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-h error
    And the refusal is the same error as for a matching h
    And relay "wss://relay.groups.example" received no event

  @designed @nip29
  Scenario: An event carrying a previous tag is refused
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries a previous tag
    When I publish that event through the group
    Then the publication is refused with a typed caller-supplied-previous error
    And the error names the previous tag
    And relay "wss://relay.groups.example" received no event
    And the signer was never asked to sign

  @designed @nip29
  Scenario: An event carrying both is refused on the first one, not silently trimmed
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    And that event carries a previous tag
    When I publish that event through the group
    Then the publication is refused with a typed error
    And neither tag was stripped from the event I supplied
    And relay "wss://relay.groups.example" received no event

  @designed @nip29
  Scenario: The group never mints a previous tag of its own
    When I publish an event of kind 9 with content "first light" through the group
    Then the delivered event carries no previous tag
    And no surface anywhere can mint a previous tag for a group publication

  @designed @nip29
  Scenario: A refused publication is distinguishable from a rejected one
    Given an unsigned event of kind 9 with content "first light"
    And that event already carries an h tag with value "photographers"
    When I publish that event through the group
    Then the refusal is reported as a caller error, not as a relay rejection
    And no receipt was created for it
