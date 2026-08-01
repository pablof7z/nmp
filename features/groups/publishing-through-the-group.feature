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

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_single_host_scope_still_routes_explicitly_to_that_one_host
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox
  # nmp:falsifier=changing Group::intent's routing away from Explicit(exactly the scope's hosts) makes a_single_host_scope_still_routes_explicitly_to_that_one_host see a different route; appending a second tag, dropping the h row, delivering to an extra relay, or widening onto the discovered author outbox makes a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox fail its tag-count, h-row, or exact-relay-set assertions
  @nip29
  Scenario: The app supplies an event and the group supplies everything else
    When I publish an event of kind 9 with content "first light" through the group
    Then the published event carries an h tag with value "photographers"
    And the published event was delivered to "wss://relay.groups.example"
    And no other relay received the published event
    And I named no relay and no tag on that call

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox
  # nmp:falsifier=having a group write fall back to, or additionally route through, the author's own discovered write relay (the indexer-fetched outbox this test proves is real and reachable) makes a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox see the outbox's admitted-event count or contact count change
  @nip29
  Scenario: A group write never reaches the author's own relays
    When I publish an event of kind 9 with content "first light" through the group
    Then relay "wss://alice-write.example" received no event
    And relay "wss://bystander.example" received no connection at all
    And the write consulted no relay list of mine

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_every_host_in_the_scope
  # nmp:evidence=rust:nmp::the_route_follows_the_group_not_whichever_key_signed_the_pre_signed_event
  # nmp:falsifier=deriving the write route from anything other than the scope's own retained host set (a caller parameter, the active identity's relay list, or the signer's own key) makes a_group_write_routes_explicitly_to_every_host_in_the_scope see a route that is not exactly the scope's hosts, and makes the_route_follows_the_group_not_whichever_key_signed_the_pre_signed_event see two differently-signed pre-signed events route differently
  @nip29
  Scenario: The route is minted by the group, not spelled by the app
    When I publish an event of kind 9 with content "first light" through the group
    Then the write's routing is explicit over exactly "wss://relay.groups.example"
    And the group minted that routing from the host it was constructed with
    And the app contributed no relay to that routing

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_reaches_the_one_publish_door
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox
  # nmp:falsifier=making Group::publish consult an indexer/relay-list fetch before accepting or routing a write makes a_group_write_reaches_the_one_publish_door hang or fail against its indexer-free bare engine, and makes the delivered-events assertions in a_group_write_routes_explicitly_to_the_whole_scope_and_never_to_the_author_outbox time out waiting on discovery that a group write must never need
  @nip29
  Scenario: A group write does not wait on the author's relay list
    Given my relay list has never been fetched
    When I publish an event of kind 9 with content "first light" through the group
    Then the published event was delivered to "wss://relay.groups.example"
    And the write never waited on a relay list
    And the write was not reported as unroutable

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::two_groups_on_two_hosts_never_bleed_into_each_other_at_the_wire
  # nmp:falsifier=hard-coding, caching, or otherwise cross-wiring the h row or route between two independently-constructed Group values makes two_groups_on_two_hosts_never_bleed_into_each_other_at_the_wire see the wrong h row or an extra delivery at either host
  @nip29
  Scenario: Two groups on two hosts never bleed into each other
    Given the group "darkroom" hosted by relay "wss://relay.darkroom.example"
    When I publish an event of kind 9 with content "first light" through the group "photographers"
    And I publish an event of kind 9 with content "still wet" through the group "darkroom"
    Then relay "wss://relay.groups.example" received only the event carrying h "photographers"
    And relay "wss://relay.darkroom.example" received only the event carrying h "darkroom"

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope
  # nmp:falsifier=flattening a multi-host write's per-relay receipts into one aggregate fact, or naming a relay outside the scope on any receipt, makes a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope see relays_named_by(...) diverge from exactly the scope's two hosts
  @nip29
  Scenario: The receipt names the host and nothing else
    When I publish an event of kind 9 with content "first light" through the group
    Then the receipt reports the event acked by "wss://relay.groups.example"
    And the receipt names no relay other than "wss://relay.groups.example"

  # nmp:id=PROTOCOL-PUBLISHINGTHROUGHTHEGROUP-007
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope
  # nmp:evidence=rust:nmp::a_host_rejection_of_a_pre_signed_event_is_an_ordinary_receipt_tied_to_its_unchanged_known_id
  # nmp:falsifier=re-routing a rejected host's write to a relay outside the scope, or suppressing the independent Acked fact from the other host, makes a_multi_host_write_preserves_exact_per_host_outcomes_without_touching_anything_outside_the_scope see relays_named_by(...) include a third relay or lose one of the two expected per-host facts; the same re-routing makes a_host_rejection_of_a_pre_signed_event_is_an_ordinary_receipt_tied_to_its_unchanged_known_id see the rejected write attempted at a second relay
  @nip29
  Scenario: A host that rejects the write says so, and nothing is tried elsewhere
    Given relay "wss://relay.groups.example" rejects every event
    When I publish an event of kind 9 with content "first light" through the group
    Then the receipt reports the event rejected by "wss://relay.groups.example"
    And the receipt carries the host's own rejection message
    And relay "wss://alice-write.example" received no event
    And the write was not re-routed to any other relay
