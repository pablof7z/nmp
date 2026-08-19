Feature: A group publishes every kind identically
  NIP-29 permits any kind to carry an h and live in a group. The group's whole
  contribution to an event is one h tag and one route; there is no kind it
  privileges, no kind it rejects, and no branch anywhere that reads the kind.
  Declaring a fixed content catalogue was a measured defect (#838); this
  invariant is what prevents it coming back in a new spelling.

  Traces to docs/internals/nip29/group-publication.md sections 4 and 7.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay

  #
  # #1245 added the one selection the read door does refuse: NIP-29's three
  # relay-signed records, which do not carry an h row at all and so cannot
  # match an h-scoped request. That is a refusal about which row identifies
  # the event, not a catalogue of what may live in a group -- and it is
  # governed on its own terms in
  # features/groups/roster-records-are-not-group-content.feature, whose
  # GROUPS-RECORDSNOTCONTENT-004 asserts that everything genuinely in a group
  # still reads through the door untouched.
  @nip29
  Scenario Outline: A chat message, a reaction and a custom kind take one path
    When I publish an event of kind <kind> with content "<content>" through the group
    Then the published event is kind <kind>
    And the published event carries an h tag with value "photographers"
    And the published event was delivered to "wss://relay.groups.example"
    And no other relay received the published event
    And the group read the kind at no point in that publication

    Examples:
      | kind  | content              |
      | 9     | first light          |
      | 7     | +                    |
      | 31337 | exposure f/8, 1/125  |

  @nip29
  Scenario: The group's only contribution to the event is the h tag
    Given an unsigned event of kind 31337 with content "exposure f/8, 1/125"
    And that event carries the tags "d"="portfolio" and "t"="landscape"
    And that event carries a created_at the app chose
    When I publish that event through the group
    Then the delivered event differs from the one I supplied only by an appended h tag
    And its kind, content and created_at survive unchanged
    And every tag I supplied survives unchanged and in the order I gave it

  @nip29
  Scenario: Kind 9 is not the group's kind
    When I publish an event of kind 9 with content "first light" through the group
    Then the group contributed no part of the kind 9 schema
    And the group exposes no composer for kind 9

  @nip29
  Scenario: An unfamiliar kind is published, not questioned
    When I publish an event of kind 44815 with content "whatever this is" through the group
    Then the published event was delivered to "wss://relay.groups.example"
    And the publication was not refused for being an unrecognised kind
