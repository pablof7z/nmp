@designed
Feature: Every NIP-29 operation crosses one typed group door
  #1122's reserved distinctions for the app-facing group door: identity,
  routing, and the operation surface's own shape. #1033 landed as
  `235bed1c` (PR #1173): a retained multi-host `RelayScope`, then a group
  id, with one ordinary engine publication/receipt path
  (`crates/nmp/src/nip29/{mod,group,predicate,read}.rs`). `Group::new`'s
  single-host constructor and `group_discovery_demand` are gone -- no alias,
  no forwarding wrapper. Every distinction below is now proved against that
  final facade.

  The door's STRUCTURE (the banned pre-#1033 spellings stay gone, the
  `RelayScope`/`Group` shape is present) and the exhaustive nine-name
  operation catalogue with its exact per-surface parameter shape currently
  have no mechanical check proving them; both were previously proved by
  scripts that are now deleted. This feature proves BEHAVIOUR: what an app
  can actually observe happen (or not happen) when it uses the door.

  The legacy `features/groups/*.feature` fixture is retired for the eight
  distinctions this file now carries as `built` -- equal-or-stronger
  governed evidence exists for all of them.

  @nip29
  Scenario: Constructing a group scope contacts nothing
    When I construct the group scope and do nothing else
    Then no relay received a connection
    And no subscription exists
    And no query was sent to any relay

  @nip29
  Scenario: A join request is publishable with no subscription at all
    Given I have never observed anything from this group
    When I publish a join request through the group
    Then the join request was delivered to the group's host
    And no subscription existed at any point during that publication
    And the publication did not require a read to succeed first

  @nip29
  Scenario: I can write into a group whose content one host refuses to let me read
    Given a host refuses my reads until I am a member
    When I observe a live query built from the group's demand for that host
    And I publish a join request through the group
    Then the join request was delivered
    And the query reports the refused read as a per-host source fact
    And the query does not report the group as empty because of it

  @nip29
  Scenario: One retained group handle mints every read and every write across its lifetime, with no lifecycle of its own
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    And I publish an event of kind 9 through the group
    And I observe a second live query built from the same group's demand for a different filter
    And I publish a second event through the same group
    Then all four operations used the same retained group handle
    And no group needed to be reconstructed between them
    And the handle owns no subscription lifecycle of its own

  @nip29
  Scenario: A moderation action the host refuses surfaces truthfully
    Given I am not an admin of the group
    And the group's host rejects the remove-user kind with a restriction message
    When I remove a user from the group
    Then the receipt reports the event rejected by that host
    And the receipt carries the host's own rejection message
    And the removal is never reported as accepted
    And no other relay was tried

  @nip29
  Scenario: A refused moderation action is reported as a relay rejection, not a guess
    Given I am not an admin of the group
    And the group's host rejects the remove-user kind with a restriction message
    When I remove a user from the group
    Then the failure is reported as a rejection by the host
    And the failure is not reported as a routing failure
    And NMP made no claim of its own about my permissions in the group

  @nip29
  Scenario: Every named operation takes semantic fields and a retained group capability, never a raw kind, tag, relay or route
    When I inspect the compiled Rust, Swift and Kotlin group operation surface
    Then each named operation's parameters are semantic fields plus a retained scope, group or author capability
    And no named operation accepts a raw kind number
    And no named operation accepts a tag name
    And no named operation accepts a relay or a route

  @nip29
  Scenario: Only deliberately modeled NIP-29 operations get named composers, on every surface
    When I inspect the group operation surface across Rust, FFI, Swift and Kotlin
    Then it offers operations only for kinds NIP-29 itself defines
    And it offers no chat composer
    And it offers no reaction composer
    And an app that wants either builds the event itself and publishes it through the group
