Feature: The app never names the host, the route, or the context tag
  The boundary stated as observable behaviour rather than as a convention. If
  any of these becomes possible again, a group stops being the only door and
  the app is back to spelling routing values it has no business knowing.

  Traces to docs/internals/nip29/group-publication.md section 8 (the boundary
  table) and to docs/internals/routing/auto-and-explicit.md section 5.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"
    And my relay list names "wss://alice-write.example" as my write relay
    And a relay "wss://bystander.example" exists that nothing references

  @designed @nip29 @must-never
  Scenario: Every relay a group write touches traces back to the group's own identity
    When I publish an event of kind 9 with content "first light" through the group
    And I publish a join request through the group
    And I remove user "3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad3bad" from the group
    Then every contacted relay is "wss://relay.groups.example"
    And the app supplied no relay anywhere in that run
    And relay "wss://bystander.example" received no connection at all

  @designed @nip29 @must-never
  Scenario: There is no way to name a relay on a group write
    When I inspect the group's write surface
    Then no group write operation accepts a relay
    And no group write operation accepts a routing value
    And a group write cannot be redirected to a relay other than its host

  @designed @nip29 @must-never
  Scenario: There is no way to set the h tag through the group
    When I inspect the group's write surface
    Then no group write operation accepts an h value
    And an event that arrives carrying its own h is refused
    And the group id given at construction is the only source of the h tag

  @designed @nip29 @must-never
  Scenario: A group write never enters the author's outbox lane
    When I publish an event of kind 9 with content "first light" through the group
    Then diagnostics show the write on an explicit route
    And diagnostics show no outbox resolution for that write
    And no relay list of mine was read for that write

  @designed @nip29 @must-never
  Scenario: A group read is pinned by the group, never widened by what the engine learns
    Given a filter selecting kind 9
    And the engine later learns of relay "wss://gossip.example" for this group's members
    When I observe a live query built from the group's demand for that filter
    Then the request is pinned to "wss://relay.groups.example"
    And relay "wss://gossip.example" received no connection at all
    And the pinned set was never widened
