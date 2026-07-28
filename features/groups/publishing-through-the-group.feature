Feature: A group publication is handed an event and nothing else
  The app holds a group built from a host and a group id, hands it an event,
  and names nothing further. The group contributes the h tag and the route to
  its own host; the author's own relays are not part of a group write at all.

  Traces to docs/internals/nip29/group-publication.md sections 1, 5 and 8, and
  to docs/internals/routing/auto-and-explicit.md sections 3 and 4.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay
    And a relay "wss://bystander.example" exists that nothing references

  @designed @nip29
  Scenario: The app supplies an event and the group supplies everything else
    When I publish an event of kind 9 with content "first light" through the group
    Then the published event carries an h tag with value "photographers"
    And the published event was delivered to "wss://relay.groups.example"
    And no other relay received the published event
    And I named no relay and no tag on that call

  @designed @nip29
  Scenario: A group write never reaches the author's own relays
    When I publish an event of kind 9 with content "first light" through the group
    Then relay "wss://alice-write.example" received no event
    And relay "wss://bystander.example" received no connection at all
    And the write consulted no relay list of mine

  @designed @nip29
  Scenario: The route is minted by the group, not spelled by the app
    When I publish an event of kind 9 with content "first light" through the group
    Then the write's routing is explicit over exactly "wss://relay.groups.example"
    And the group minted that routing from the host it was constructed with
    And the app contributed no relay to that routing

  @designed @nip29
  Scenario: A group write does not wait on the author's relay list
    Given my relay list has never been fetched
    When I publish an event of kind 9 with content "first light" through the group
    Then the published event was delivered to "wss://relay.groups.example"
    And the write never waited on a relay list
    And the write was not reported as unroutable

  @designed @nip29
  Scenario: Two groups on two hosts never bleed into each other
    Given the group "darkroom" hosted by relay "wss://relay.darkroom.example"
    When I publish an event of kind 9 with content "first light" through the group "photographers"
    And I publish an event of kind 9 with content "still wet" through the group "darkroom"
    Then relay "wss://relay.groups.example" received only the event carrying h "photographers"
    And relay "wss://relay.darkroom.example" received only the event carrying h "darkroom"

  @designed @nip29
  Scenario: The receipt names the host and nothing else
    When I publish an event of kind 9 with content "first light" through the group
    Then the receipt reports the event acked by "wss://relay.groups.example"
    And the receipt names no relay other than "wss://relay.groups.example"

  @designed @nip29
  Scenario: A host that rejects the write says so, and nothing is tried elsewhere
    Given relay "wss://relay.groups.example" rejects every event
    When I publish an event of kind 9 with content "first light" through the group
    Then the receipt reports the event rejected by "wss://relay.groups.example"
    And the receipt carries the host's own rejection message
    And relay "wss://alice-write.example" received no event
    And the write was not re-routed to any other relay
