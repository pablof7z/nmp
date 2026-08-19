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

  Traces to #1033 and #1252, and to `crates/nmp/src/nip29/{mod,group,predicate}.rs`.

  Background:
    Given a NIP-29 relay scope named over more than one relay

  @nip29
  Scenario: An app-supplied relay set can be empty, so naming relays is fallible
    Given an app names no relay at all
    When the app calls the relay-scope door
    Then the door refuses with a typed empty-relay-set error
    And no relay scope, and therefore no group, is ever constructed from it

  @nip29
  Scenario: Duplicate or reordered relays name the same scope
    Given an app names the same two relays twice, in different orders
    When the app calls the relay-scope door for each ordering
    Then both calls produce the identical canonical relay scope

  @nip29
  Scenario: A group write routes explicitly to every host the scope names, one host or many
    Given a group narrowed from that relay scope
    When the app publishes an event through the group
    Then the write's route names every host in the scope, in canonical order
    And the write's route names no host outside the scope
    And a scope naming exactly one relay routes explicitly to that one relay alone

  @nip29
  Scenario: An unsigned group write freezes the exact author the app named
    Given a group narrowed from that relay scope
    When the app publishes an event through the group as a named author
    Then the write's identity is that exact author, not whichever account happens to be active later

  @nip29
  Scenario: A multi-host group read is one live query, one complete branch per host
    Given a group narrowed from that relay scope
    When the app reads an app-chosen selection through the group
    Then the result is one ordinary live query
    And it declares exactly one branch per host in the scope
    And each branch is pinned to its own host alone and scoped to the group's own h row

  @nip29
  Scenario: Every NIP-29-owned nesting level is pinned to its own branch host, never inherited
    Given a group-discovery predicate asking which groups name a subject as a member
    When the scope lowers that predicate once per host, for a two-host scope
    Then the outer per-host listing at each host is pinned to that host alone
    And the nested member-list evidence inside it is ALSO pinned to that same host alone
    And neither level is pinned to the other host or to both hosts at once

  @nip29
  Scenario: A multi-host discovery listing is also one live query, one branch per host
    Given a group-discovery predicate asking which groups name a subject as a member
    When the app asks the scope for groups matching that predicate, over a two-host scope
    Then the result is one ordinary live query with exactly one branch per host
    And each relay is asked only for the records the app actually named

  @nip29
  Scenario: The same predicate lowered at two different hosts yields two independent values
    Given a group-discovery predicate asking which groups name a subject as a member
    When the scope lowers that predicate at each of two different hosts
    Then the two lowered values are pinned to their own host and are not equal to each other

  @nip29
  Scenario: A predicate nested inside a caller-owned lookup never has that lookup's authority overwritten
    Given a discovery predicate for groups whose admins include the app's own follows
    When the scope lowers that predicate at one host
    Then the admin-list evidence NIP-29 owns is pinned to that host
    And the nested follows lookup the app owns keeps its own original authority, unrewritten

  @nip29
  Scenario: A discovery predicate built from the current account stays reactive after lowering
    Given a group-discovery predicate asking which groups name the current account as a member
    When the scope lowers that predicate at one host
    Then the lowered query still asks for whichever account is active, not a frozen pubkey

  @nip29
  Scenario: Discovery predicates compose with the grammar's own set algebra
    Given a "member of this group" predicate and an "admin of this group" predicate
    When the app unions, intersects, or subtracts them
    Then the composed predicate lowers to the grammar's ordinary set-operation binding
    And no second, NIP-29-specific combinator vocabulary is introduced

  @nip29
  Scenario: A directory asks a relay which groups it advertises, and names no group ids of its own
    Given an app browsing the rooms a relay advertises rather than watching rooms it already knows
    When the app observes with the unconstrained predicate over a two-host scope
    Then each host's branch selects the relay-signed records the app named
    And no branch carries a group-id row at all
    And the app's own per-host bound is the only thing bounding the answer

  @nip29
  Scenario: The named discovery leaves are shorthands over the general query language, not a closed vocabulary
    Given the general spelling that takes an ordinary live-query filter over a relay-signed group record
    When the app writes the member-list question out in full instead of using the named leaf
    Then the two are the same value, not merely equivalent
    And a question no named leaf spells is expressible through the same door

  @nip29
  Scenario: An app watches the groups named in its own saved list, without re-deriving them by hand
    Given an app whose room list is its own saved-groups event rather than a set of ids typed into the code
    When the app names that lookup as the source of the group ids
    Then the observation follows the saved list as it changes, with no second observation
    And the lookup resolves from the app's own relays, never from the group's hosts

  @nip29
  Scenario: The general spelling may only name records the group's host is authoritative for
    Given a filter naming a kind that is not one of NIP-29's three relay-signed group records
    When the app tries to build a group-id source from it
    Then the door refuses with a typed error naming the offending kind
    And no observation is opened that would silently ask the wrong relay
