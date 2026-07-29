Feature: Reading a group goes through the one read door
  A group does not hand out streams. It mints an ordinary query, and the app
  takes that query through the same subscription door as every other read in
  NMP. A second door onto the same mechanism is exactly the shape #838 deleted
  on the write side, and it is not rebuilt here on the read side.

  The app decides which kinds it wants. The group contributes the host pinning
  and the h scoping and nothing else -- there is no catalogue of "the group's
  kinds", because any kind can carry an h.

  Traces to docs/internals/nip29/group-publication.md sections 2, 3 and 4.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  @nip29
  Scenario: The group mints a query and the ordinary subscription door observes it
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then a subscription is returned by the same observe call every other read uses
    And the request is pinned to "wss://relay.groups.example"
    And the request is scoped to h "photographers"
    And the request selects exactly kind 9
    And no relay outside "wss://relay.groups.example" was asked

  @nip29
  Scenario: There is no second way to observe a group
    When I inspect the group's read surface
    Then the group exposes no observe operation of its own
    And the group exposes no stream, channel or callback of its own
    And every group read in the surface passes through the same observe call

  @nip29
  Scenario Outline: The app chooses the kinds; the group imposes no catalogue
    Given a filter selecting <kinds>
    When I observe a live query built from the group's demand for that filter
    Then the request selects exactly <kinds>
    And the request is pinned to "wss://relay.groups.example"
    And the request is scoped to h "photographers"
    And the group contributed no kind of its own to the request

    Examples:
      | kinds                |
      | kind 9               |
      | kinds 9 and 9000     |
      | kind 30315           |
      | kind 7               |
      | kind 39002           |
      | kind 31337           |

  @nip29
  Scenario: One group serving four simultaneous queries is the normal case
    Given a chat filter selecting kinds 9 and 9000 and 9001
    And an activity filter selecting kind 30315
    And a reactions filter selecting kind 7
    And a membership filter selecting kinds 39002 and 39001
    When I observe live queries built from the group's demand for all four filters
    Then four independent subscriptions exist at once
    And each request is pinned to "wss://relay.groups.example"
    And each request is scoped to h "photographers"
    And the same group instance minted all four
    And no group needed to be reconstructed between them

  @nip29
  Scenario: Two groups on the same host stay separated by their h scoping
    Given the group "darkroom" also hosted by relay "wss://relay.groups.example"
    And relay "wss://relay.groups.example" holds a kind 9 event with h "photographers" saying "first light"
    And relay "wss://relay.groups.example" holds a kind 9 event with h "darkroom" saying "still wet"
    And a filter selecting kind 9
    When I observe a live query built from the "photographers" group's demand for that filter
    Then the query shows only "first light"

  @nip29
  Scenario: The host is a query-declared pinning, not a directory fact
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then diagnostics attribute "wss://relay.groups.example" to the query's own pinned source
    And diagnostics attribute it to no relay-list or operator-configured fact
    And per-source acquisition evidence is reported for "wss://relay.groups.example"

  @nip29
  Scenario: An unreachable host does not make the group look empty
    Given relay "wss://relay.groups.example" cannot connect
    And a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then the query shows no events
    And the query does not claim its empty result is complete
    And the acquisition evidence reports the host as unreachable
