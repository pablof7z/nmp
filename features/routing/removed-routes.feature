Feature: The routes that were removed stay removed
  Three shipped routing spellings die with this design, and none of them comes
  back as an alias, a shim, or a compatibility decoder: "no backwards
  compatibility!!!! I told you this so many times!!!"

  - `AuthorOutbox` becomes the built-in behaviour of `Auto` -- and gains the
    p-tag fan-out and app relays the variant never had.
  - `PrivateNarrow` was "exactly these relays, directory-blind, fail closed
    when empty", which IS `Explicit`. Its invariants survive; its name and its
    privacy framing do not, because fail-closed is a routing property and
    privacy was only one reason to want it.
  - `RelayListBootstrap` existed to deliver a kind:10002 before the author's
    relay list was known. That is an `Explicit` minted by `nmp-nip65`, the same
    pattern any protocol crate uses, and no dedicated variant earns its place.

  Two never-built names are tombstoned alongside them: `GroupHost` (a redundant
  spelling of `Explicit([host])`, whose authority newtype was rejected outright
  -- "bare. It is not only overengineering; it's wrong for many other
  reasons"), and `AuthorRelayList(Kind)`, which was a partial spelling of
  `Auto` with the kind hoisted into the enum.

  The removals themselves are enforced rather than promised (#1105):
  `scripts/check-routing-vocabulary.sh` enumerates the Rust, FFI, Swift and
  Kotlin routing surfaces and requires exactly the two words, and tombstones
  every retired spelling above -- `GroupHost` and `AuthorRelayList`
  included -- with the replacement each maps to.
  `crates/nmp/tests/group_publication_door.rs` is the group door's runtime
  proof: the app supplies content only, the host alone receives, and the
  author's own discovered outbox is never contacted. Scenarios still tagged
  `@designed` remain acceptance criteria for the parts that are not built
  (`docs/internals/routing/removed-routes.md`).

  Background:
    Given I am logged in as my own account
    And my relay list names "outbox-a" as my write relay

  # ---- nothing to say them with ----------------------------------------

  @designed
  Scenario Outline: A retired routing has no app-reachable spelling
    # Not "is discouraged" and not "is deprecated": there is no value an app
    # can construct that means any of these, on any platform. The replacement
    # column is what the caller says instead -- and in every case it is one of
    # the two words, which is the point.
    When an app looks for a way to say "<retired>"
    Then no such routing exists on the Rust, Swift, or Kotlin surface
    And what it says instead is "<replacement>"

    Examples:
      | retired               | replacement                          |
      | author-outbox         | figure it out                        |
      | private-narrow        | these exact relays                   |
      | relay-list-bootstrap  | these exact relays, minted by nmp-nip65 |
      | group-host            | these exact relays, minted by nmp-nip29 |
      | author-relay-list     | figure it out                        |

  @designed
  Scenario: There is no third routing word to reach for
    # The standing guard on the surface itself. A third word appearing in an
    # API review is a design regression against a settled ruling, whatever it
    # is called and however local its motivation.
    When an app enumerates every routing it can express
    Then it finds exactly "figure it out" and "these exact relays"
    And it finds nothing that names a NIP
    And it finds nothing that names a strategy

  @designed
  Scenario: A group write still crosses the app surface only through the group door
    # What the reversal must NOT have loosened. `Explicit` being general does
    # not mean an app hand-routes group writes: "the app shouldn't say 'publish
    # to group x, relay y', it should create a 'group' object [...] and
    # group.publish(event_builder_stuff) would take care of adding the h and
    # publishing to the correct relay."
    Given a group hosted by "group-host"
    When I publish a note saying "hi group" through that group
    Then the note is delivered to "group-host"
    And the app never named "group-host"
    And the app never wrote the group tag itself
    And "outbox-a" was never contacted

  # ---- old journal rows ------------------------------------------------

  @designed
  Scenario Outline: A durable write journalled under an old spelling is kept, not reinterpreted
    # The retained-but-unreadable rule, which is what "no backwards
    # compatibility" means for data that already exists on disk. An obligation
    # written under a routing this build cannot read is preserved exactly as
    # written and never resolved -- guessing that "author-outbox" meant `Auto`
    # would republish an old write to relays nobody chose for it, which is
    # worse than leaving it inert. This is the shape the restart falsifier
    # already proves for `pinned-host-hex`
    # (`crates/nmp/tests/durable_accepted_restart.rs`).
    Given a durable store holding an accepted write whose routing is spelled "<old>"
    When I reconstruct the engine from that durable store
    Then the row is retained exactly as written
    And it is not read as "figure it out"
    And it is not read as "these exact relays"
    And no relay is contacted for it
    And it never publishes

    Examples:
      | old                 |
      | author-outbox       |
      | private-narrow-hex  |
      | nip65-bootstrap-hex |
      | pinned-host-hex     |

  @designed
  Scenario: An unreadable old row does not stop the writes around it
    # Retention must be inert, not obstructive: a store carrying one
    # unreadable obligation still recovers every readable one and still
    # accepts new writes.
    Given a durable store holding an accepted write whose routing is spelled "author-outbox"
    And that same store holds an accepted write routed to exactly "chosen-relay"
    When I reconstruct the engine from that durable store
    Then the write routed to "chosen-relay" is delivered to "chosen-relay"
    And the write spelled "author-outbox" never publishes
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the note is delivered to "outbox-a"

  # ---- what the removals must not have taken with them -----------------

  Scenario: Fail-closed survived the removal of the private route
    # `PrivateNarrow`'s discipline transfers to `Explicit` intact: verbatim
    # execution, no widen path, and a directory that is never consulted. What
    # does NOT transfer is the privacy wording -- a group host is a public
    # target, and a journal describing that write as "private" would be lying.
    Given the directory knows "outbox-a" and "outbox-b" as my write relays
    When I publish a note saying "narrow" to exactly "chosen-relay"
    Then the note is delivered to "chosen-relay"
    And no relay outside "chosen-relay" was ever contacted
    And nothing describes that write as private

  @designed
  Scenario: Bootstrapping a relay list still works without a route of its own
    # The case `RelayListBootstrap` existed for, served by the general
    # primitive: publishing my own kind:10002 when nobody knows where I write
    # is a crate minting exact relays, not a variant.
    Given my relay list has never been fetched
    When nmp-nip65 publishes my relay list to exactly "bootstrap-relay"
    Then the relay list is delivered to "bootstrap-relay"
    And the receipt reports routing complete
    And no discovery was needed to route it
