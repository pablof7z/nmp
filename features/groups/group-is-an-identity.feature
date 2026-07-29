Feature: A group is an identity, not a subscription
  A group is built from a host and a group id and exists on its own. This is
  load-bearing rather than incidental: a join request means writing into a
  group you cannot read yet, so the write door must not require a read to
  exist first. The same instance lives for the whole room lifetime and mints
  every query and every write for it.

  Traces to docs/internals/nip29/group-publication.md section 2.

  Background:
    Given the group "photographers" hosted by relay "wss://relay.groups.example"
    And I am logged in as "a1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1cea1ce"

  @nip29
  Scenario: Constructing a group contacts nothing
    When I construct the group and do nothing else
    Then no relay received a connection
    And no subscription exists
    And no query was sent to "wss://relay.groups.example"

  @nip29
  Scenario: A join request is publishable with no subscription at all
    Given I have never observed anything from this group
    When I publish a join request through the group
    Then the join request was delivered to "wss://relay.groups.example"
    And no subscription existed at any point during that publication
    And the publication did not require a read to succeed first

  @nip29
  Scenario: I can write into a group whose content I am not allowed to read
    Given relay "wss://relay.groups.example" refuses my reads until I am a member
    And a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    And I publish a join request through the group
    Then the join request was delivered to "wss://relay.groups.example"
    And the query reports the refused read as a source fact
    And the query does not report the group as empty

  @nip29
  Scenario: One group instance serves reads and writes across the room's lifetime
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    And I publish an event of kind 9 with content "first light" through the group
    And I observe a second live query built from the same group's demand for a filter selecting kind 30315
    And I publish an event of kind 7 with content "+" through the group
    Then all four operations used the same group instance
    And no group had to be reconstructed between them
    And every one of them named "wss://relay.groups.example" without the app supplying it
