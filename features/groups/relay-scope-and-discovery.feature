Feature: A group can live on more than one relay at once
  #1033 replaced the single-host `Group` door with `nip29::on(hosts)`, a
  `RelayScope` an app narrows to one group. NIP-29 authority is per-relay, not
  per-group: two relays hosting the same group id are two independent groups
  with the same name, so a multi-relay group's write must reach EVERY host in
  its scope and its read must never let evidence observed at one host answer a
  question about another.

  These scenarios are governed under #1074 rather than executed by the
  transitional `nmp-bdd` mechanism runner: the exact behavior they describe is
  already proved, red-then-green, by the `nmp-nip29`/`nmp` unit falsifiers
  they cite. Un-tagging (removing every `@wip`/`@designed`) is the definition
  of done per #979, and these scenarios were never tagged that way -- they are
  born `built`.

  Traces to #1033 and to `crates/nmp/src/nip29/{mod,group,predicate}.rs`.

  Background:
    Given a NIP-29 relay scope named over more than one relay

  # nmp:id=GROUPS-RELAYSCOPE-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_empty_relay_set_forms_no_scope
  # nmp:falsifier=Delete the `hosts.is_empty()` guard from `nip29::on`; a caller-supplied empty relay set would then construct a scope backed by no relay at all instead of the typed `RelayScopeError::EmptyRelaySet` refusal.
  @nip29
  Scenario: An app-supplied relay set can be empty, so naming relays is fallible
    Given an app names no relay at all
    When the app calls the relay-scope door
    Then the door refuses with a typed empty-relay-set error
    And no relay scope, and therefore no group, is ever constructed from it

  # nmp:id=GROUPS-RELAYSCOPE-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::duplicate_and_unsorted_hosts_canonicalize_to_one_sorted_set
  # nmp:falsifier=Collect the caller's hosts into a `Vec` instead of a `BTreeSet`; a scope built from permuted or repeated relay input would then compare unequal to the same relays named once, in order.
  @nip29
  Scenario: Duplicate or reordered relays name the same scope
    Given an app names the same two relays twice, in different orders
    When the app calls the relay-scope door for each ordering
    Then both calls produce the identical canonical relay scope

  # nmp:id=GROUPS-RELAYSCOPE-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_write_routes_explicitly_to_every_host_in_the_scope
  # nmp:evidence=rust:nmp::a_single_host_scope_still_routes_explicitly_to_that_one_host
  # nmp:falsifier=Route a group write to `WriteRouting::Auto`, or to only the first host in the scope, instead of `Explicit(every scope host)`; the multi-host case would then reach one relay silently while claiming to reach all of them.
  @nip29
  Scenario: A group write routes explicitly to every host the scope names, one host or many
    Given a group narrowed from that relay scope
    When the app publishes an event through the group
    Then the write's route names every host in the scope, in canonical order
    And the write's route names no host outside the scope
    And a scope naming exactly one relay routes explicitly to that one relay alone

  # nmp:id=GROUPS-RELAYSCOPE-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::an_unsigned_group_write_freezes_the_exact_author
  # nmp:falsifier=Resolve a group write's author from whichever account is active when the publish door accepts the intent, rather than from the exact `PublicKey` the app passed to `Group::publish`; switching the active account between composing and acceptance would then change who the event is signed by.
  @nip29
  Scenario: An unsigned group write freezes the exact author the app named
    Given a group narrowed from that relay scope
    When the app publishes an event through the group as a named author
    Then the write's identity is that exact author, not whichever account happens to be active later

  # nmp:id=GROUPS-RELAYSCOPE-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_group_read_is_one_complete_branch_per_host
  # nmp:evidence=rust:nmp::a_multi_host_read_is_one_live_query_with_one_branch_per_host
  # nmp:falsifier=Collapse a multi-host group read into one `Demand` pinned to `Pinned({every host})` instead of one `LiveQuery` branch per host; a row genuinely observed only at host A would then read as evidence for host B too.
  @nip29
  Scenario: A multi-host group read is one live query, one complete branch per host
    Given a group narrowed from that relay scope
    When the app reads an app-chosen selection through the group
    Then the result is one ordinary live query
    And it declares exactly one branch per host in the scope
    And each branch is pinned to its own host alone and scoped to the group's own h row

  # nmp:id=GROUPS-DISCOVERY-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::scope_stamps_exact_hosts_on_every_nested_nip29_demand
  # nmp:falsifier=Verified red-then-green in `nmp_nip29::discovery::list_evidence_at` by replacing one inner pin (`pinned_public_at`) with `Demand::from_filter`: `assertion left == right failed: depth 1 (the member-list evidence) must be pinned to wss://host-1.example.com alone, not inherited and not cross-hosted -- left: Public, right: Pinned({RelayUrl("wss://host-1.example.com")})`.
  @nip29
  Scenario: Every NIP-29-owned nesting level is pinned to its own branch host, never inherited
    Given a group-discovery predicate asking which groups name a subject as a member
    When the scope lowers that predicate once per host, for a two-host scope
    Then the outer per-host listing at each host is pinned to that host alone
    And the nested member-list evidence inside it is ALSO pinned to that same host alone
    And neither level is pinned to the other host or to both hosts at once

  # nmp:id=GROUPS-DISCOVERY-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_multi_host_listing_is_one_live_query_with_one_branch_per_host
  # nmp:falsifier=Fold a multi-host discovery listing's branches into fewer live queries than hosts, or drop a host's branch on refusal; either would silently under-resolve which groups the app is shown as belonging to.
  @nip29
  Scenario: A multi-host discovery listing is also one live query, one branch per host
    Given a group-discovery predicate asking which groups name a subject as a member
    When the app asks the scope for groups matching that predicate, over a two-host scope
    Then the result is one ordinary live query with exactly one branch per host

  # nmp:id=GROUPS-DISCOVERY-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::two_hosts_lower_to_two_independently_pinned_values
  # nmp:falsifier=Cache or memoize a predicate's lowered form across hosts instead of closing it fresh per host; the value lowered for host A would then leak into, or replace, the value lowered for host B.
  @nip29
  Scenario: The same predicate lowered at two different hosts yields two independent values
    Given a group-discovery predicate asking which groups name a subject as a member
    When the scope lowers that predicate at each of two different hosts
    Then the two lowered values are pinned to their own host and are not equal to each other

  # nmp:id=GROUPS-DISCOVERY-004
  # nmp:status=built
  # nmp:evidence=rust:nmp::a_caller_owned_inner_demand_keeps_its_own_authority_at_depth_two
  # nmp:falsifier=Have predicate lowering recursively repin every nested binding to the branch host, including a caller-supplied one; an app's own kind:3 follows lookup nested inside `admin_list_includes` would then be asked of the group's hosts, which have no reason to hold that app's contact list, and the app would silently see fewer admins than it actually follows.
  @nip29
  Scenario: A predicate nested inside a caller-owned lookup never has that lookup's authority overwritten
    Given a discovery predicate for groups whose admins include the app's own follows
    When the scope lowers that predicate at one host
    Then the admin-list evidence NIP-29 owns is pinned to that host
    And the nested follows lookup the app owns keeps its own original authority, unrewritten

  # nmp:id=GROUPS-DISCOVERY-005
  # nmp:status=built
  # nmp:evidence=rust:nmp::the_active_pubkey_stays_reactive_through_lowering
  # nmp:falsifier=Flatten `Binding::Reactive(IdentityField::ActivePubkey)` to a literal pubkey at the moment a predicate is lowered; a discovery query built before an account switch would then keep answering for the account that was active when it was built, rather than the one active now.
  @nip29
  Scenario: A discovery predicate built from the active account stays reactive after lowering
    Given a group-discovery predicate asking which groups name the active account as a member
    When the scope lowers that predicate at one host
    Then the lowered query still asks for whichever account is active, not a frozen pubkey

  # nmp:id=GROUPS-DISCOVERY-006
  # nmp:status=built
  # nmp:evidence=rust:nmp::set_algebra_composes_predicates_into_ordinary_bindings
  # nmp:evidence=rust:nmp::union_and_diff_fold_with_the_grammars_own_algebra
  # nmp:falsifier=Give `GroupPredicate::union`/`intersect`/`minus` a second, NIP-29-specific combinator representation instead of folding into the grammar's own `Binding::SetOp`; a composed predicate would then need its own resolver path instead of reusing the one every other composite query already has.
  @nip29
  Scenario: Discovery predicates compose with the grammar's own set algebra
    Given a "member of this group" predicate and an "admin of this group" predicate
    When the app unions, intersects, or subtracts them
    Then the composed predicate lowers to the grammar's ordinary set-operation binding
    And no second, NIP-29-specific combinator vocabulary is introduced
