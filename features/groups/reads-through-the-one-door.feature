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

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-001
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_read_branch_pins_the_host_and_scopes_the_app_supplied_selection
  # nmp:evidence=rust:nmp-nip29::a_read_selection_that_already_constrains_h_is_refused
  # nmp:evidence=rust:nmp::the_read_half_is_a_live_query_the_ordinary_observe_door_takes
  # nmp:falsifier=copying only part of the app's selection (or dropping/altering the appended h row) in group_demand_at makes a_read_branch_pins_the_host_and_scopes_the_app_supplied_selection see a wrong kind set, source, or #h binding; silently overwriting instead of refusing a caller-supplied #h makes a_read_selection_that_already_constrains_h_is_refused return Ok instead of CallerSuppliedContextConstraint; and deleting Group::read or its wiring into Engine::observe makes the_read_half_is_a_live_query_the_ordinary_observe_door_takes fail to open a subscription
  @nip29
  Scenario: The group mints a query and the ordinary subscription door observes it
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then a subscription is returned by the same observe call every other read uses
    And the request is pinned to "wss://relay.groups.example"
    And the request is scoped to h "photographers"
    And the request selects exactly kind 9
    And no relay outside "wss://relay.groups.example" was asked

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-002
  # nmp:status=built
  # nmp:evidence=script:repository::scripts/check-nip29-surfaces.sh
  # nmp:falsifier=Give a group or relay-scope value a read lifecycle of its own on any of the surfaces this script scans -- open a socket, hold a relay pool, add a reconnect or retry loop beside the projection, or stop routing the records observation through the engine's own subscription -- and check-nip29-surfaces.sh fails with "a group value grew a read lifecycle of its own" or "no longer opens the engine's own subscription". #1233 narrowed this from banning the WORD observe: that banned the group-records projection along with the defect, and the defect it is aimed at is a parallel lifecycle onto the same mechanism, not a typed reader over it. #1653 moved this claim's mechanism here from the deleted scripts/check-nip29-read-door.sh, which had its own top-level workflow file; the check itself is unchanged in substance, only relocated and ported from a hardcoded facade-file array to a glob that also covers groups.rs.
  @nip29
  Scenario: A group owns no way of its own to reach a relay
    When I inspect the group's read surface
    Then the group opens no connection of its own
    And the group holds no retry or reconnection policy of its own
    And the one live projection it offers is driven by the same engine subscription every other read uses
    And withdrawing that projection withdraws exactly the demand the engine opened for it

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-003
  # nmp:status=built
  # nmp:evidence=rust:nmp-nip29::a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections
  # nmp:falsifier=having group_demand_at substitute, filter, or reject any kind set instead of copying the caller's kinds through unread makes a_read_branch_imposes_no_kind_catalogue_over_arbitrary_app_selections see a kind set other than the app's own for at least one of the six cases in its table
  #
  # kind 39002 was a row in this table until #1245. It should never have been:
  # a kind:39002 event does not carry an h row at all, so that row was asserting
  # that a request no event could match was built faithfully. The three
  # relay-signed records now have their own door and their own refusal
  # (features/groups/roster-records-are-not-group-content.feature); kind 9022,
  # which NIP-29 defines and which genuinely does live in a group, takes its
  # place in the table.
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
      | kind 9022            |
      | kind 31337           |

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::one_group_value_mints_several_independent_simultaneous_observations
  # nmp:falsifier=capping Group to one live observation, or making a second/third/fourth read or records observation on the same group value fail or silently replace an earlier one, makes one_group_value_mints_several_independent_simultaneous_observations see fewer than four simultaneously open subscriptions
  @nip29
  Scenario: One group serving four simultaneous queries is the normal case
    Given a chat filter selecting kinds 9 and 9000 and 9001
    And an activity filter selecting kind 30315
    And a reactions filter selecting kind 7
    And a watch on the group's admin and member records
    When I open all four at once from the same group value
    Then four independent subscriptions exist at once
    And each request is pinned to "wss://relay.groups.example"
    And each request is scoped to h "photographers"
    And the same group instance minted all four
    And no group needed to be reconstructed between them

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::two_group_ids_on_one_host_differ_only_in_their_h_branch
  # nmp:evidence=rust:nmp::two_group_ids_on_the_same_host_stay_separated_by_h_at_the_wire
  # nmp:falsifier=dropping the per-group #h scoping from group_demand_at makes two_group_ids_on_one_host_differ_only_in_their_h_branch see identical branches for two different group ids on the same host, and makes two_group_ids_on_the_same_host_stay_separated_by_h_at_the_wire see the other group's own event leak into this group's subscription
  @nip29
  Scenario: Two groups on the same host stay separated by their h scoping
    Given the group "darkroom" also hosted by relay "wss://relay.groups.example"
    And relay "wss://relay.groups.example" holds a kind 9 event with h "photographers" saying "first light"
    And relay "wss://relay.groups.example" holds a kind 9 event with h "darkroom" saying "still wet"
    And a filter selecting kind 9
    When I observe a live query built from the "photographers" group's demand for that filter
    Then the query shows only "first light"

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_read_never_widens_beyond_its_pinned_host_to_a_discovered_author_outbox
  # nmp:falsifier=sourcing a group read's Demand from SourceAuthority::AuthorOutboxes instead of Pinned makes a_group_read_never_widens_beyond_its_pinned_host_to_a_discovered_author_outbox see the discovered author outbox relay contacted alongside or instead of the retained pinned host
  @nip29
  Scenario: The host is a query-declared pinning, not a directory fact
    Given a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then diagnostics attribute "wss://relay.groups.example" to the query's own pinned source
    And diagnostics attribute it to no relay-list or operator-configured fact
    And per-source acquisition evidence is reported for "wss://relay.groups.example"

  # nmp:id=PROTOCOL-READSTHROUGHTHEONEDOOR-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unproven_host_never_presents_a_group_read_as_authoritatively_empty
  # nmp:falsifier=the same SourceAuthority::AuthorOutboxes substitution above (with no active identity and no routing facts registered) makes an_unproven_host_never_presents_a_group_read_as_authoritatively_empty see a non-empty shortfall -- the "nothing is even trying" fact -- instead of the honest single Connecting source this scenario requires
  @nip29
  Scenario: An unreachable host does not make the group look empty
    Given relay "wss://relay.groups.example" cannot connect
    And a filter selecting kind 9
    When I observe a live query built from the group's demand for that filter
    Then the query shows no events
    And the query does not claim its empty result is complete
    And the acquisition evidence reports the host as unreachable
