Feature: A pre-signed event is published unchanged, and its h is validated
  Some apps sign first, take the exact event id, arm an observation on it, and
  only then publish. That path cannot append anything: appending an h would
  change the bytes and therefore the id. So on the pre-signed path the group
  VALIDATES the h that is already there. A missing or wrong h is a typed
  refusal -- never a silent repair, never a re-sign.

  Traces to docs/internals/nip29/group-publication.md section 6.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay

  @designed @nip29
  Scenario: A correctly contextualised signed event goes out byte for byte
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    When I publish that signed event through the group
    Then the delivered event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And its signature is byte-identical to the one I supplied
    And no tag was added, removed or reordered
    And the signer was never asked to sign
    And it was delivered to "wss://relay.groups.example" and to no other relay

  @designed @nip29
  Scenario: The id is known before publication, so an observation can be armed on it
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And I am observing a live query for exactly that id
    When I publish that signed event through the group
    Then the query for that id matches the event that reached "wss://relay.groups.example"

  @designed @nip29
  Scenario: A signed event with no h is refused, not repaired
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries no h tag
    When I publish that signed event through the group
    Then the publication is refused with a typed missing-group-context error
    And no h tag was appended to it
    And its id was never recomputed
    And relay "wss://relay.groups.example" received no event

  @designed @nip29
  Scenario: A signed event carrying another group's h is refused, and the error says both
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "still wet"
    And that signed event carries an h tag with value "darkroom"
    When I publish that signed event through the group "photographers"
    Then the publication is refused with a typed mismatched-group-context error
    And the error names both "darkroom" and "photographers"
    And no relay received the event

  @designed @nip29
  Scenario: A signed event with more than one h tag is refused
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries h tags with values "photographers" and "darkroom"
    When I publish that signed event through the group
    Then the publication is refused with a typed ambiguous-group-context error
    And relay "wss://relay.groups.example" received no event

  @designed @nip29
  Scenario: The route follows the group, not the signature
    Given an event signed earlier by "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" of kind 9 with content "not mine"
    And that signed event carries an h tag with value "photographers"
    And "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0" names "wss://bob-write.example" as their write relay
    When I publish that signed event through the group
    Then it was delivered to "wss://relay.groups.example" and to no other relay
    And relay "wss://bob-write.example" received no event
    And relay "wss://alice-write.example" received no event
    And the signature still belongs to "b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0b0"

  @designed @nip29
  Scenario: A pre-signed publication that the host rejects keeps its id
    Given an event signed earlier by "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce" of kind 9 with content "first light"
    And that signed event carries an h tag with value "photographers"
    And that signed event has id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And relay "wss://relay.groups.example" rejects every event
    When I publish that signed event through the group
    Then the receipt reports the event rejected by "wss://relay.groups.example"
    And the receipt is addressed by the same id "9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c9f2c"
    And the event was not re-signed and not re-routed
