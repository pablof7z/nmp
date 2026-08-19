Feature: Where an ordinary event goes when the app says nothing
  "Publish this event" is the whole of what an app has to say. Everything
  after that is the built-in outbox resolver's business, and outbox is not one
  source. Its intended answer is the union of four evidence-owned
  contributions:

    1. the author's neutral outbound routes,
    2. the operator-configured app relays, always,
    3. every p-tagged recipient's neutral inbound routes -- their inbox,
    4. verified relay provenance NMP recorded for the referenced parent event.

  Pablo, on what the app surface is allowed to be:

  > the app should be able to say "publish this event" and it would default to using outbox.

  The first three contributions come from `RoutingFacts`. The fourth resolves
  the signed event's direct reply-parent id through NMP's canonical thread
  grammar, then reads that canonical row's verified relay provenance. A reply
  can therefore publish to known author/app/recipient lanes immediately while
  author-route discovery is still open, without losing the relay context where
  NMP actually found the conversation.

  Parent provenance is NMP's own observation, not arbitrary text in a tag. A
  relay hint authored into an `e` tag does not become verified merely because
  it survived signing. The contrast at the end of this feature preserves both
  halves of the correction. It deliberately uses one observed parent source;
  choosing among several verified sources is a separate policy decision.

  `RoutingFacts` still owns the neutral author directions: one atomic author
  fact owns both directional sets, while operator app and fallback sets remain
  independent policy. Outbox invents no protocol kind, discovery source, or
  implicit indexer lane.

  The recipient half is the one that is easy to get subtly wrong, and the
  resolver says so where it does it: a recipient is reached at their READ
  relays, NEVER their write relays. Delivering to the relays someone PUBLISHES
  to is a message they will never read, and the two sets are routinely
  disjoint.

  Background:
    Given I am logged in as my own account
    And my relay list names "author-write-1" and "author-write-2" as my write relays

  # ---- the author's own outbox -----------------------------------------

  Scenario: An event addressed to nobody still reaches two sources, not one
    # The floor case, and already more than master does: with nobody p-tagged
    # there is no fan-out to compute, and the answer is STILL the union of the
    # author's write relays and the app relays. Source 2 is not a fallback for
    # a thin source 1 -- it is unconditional, which is why it appears here
    # where source 1 is perfectly healthy.
    Given app relays "app-indexer-1" and "app-indexer-2" are configured
    When I publish a note saying "hello, no one in particular"
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer-1", and "app-indexer-2"
    And routing is complete

  Scenario: The author is reached at their write relays, never their read relays
    # The mirror image of the recipient rule below, and the reason both halves
    # have to be stated: an author fact has two sets, and outbox reads a
    # DIFFERENT one depending on whether the identity is the author or an addressee. An
    # author's read-marked relay is where they collect mail, not where they
    # publish; routing a note there tells nobody anything.
    Given my relay list also names "author-read-only" as a read-marked relay
    And no app relays are configured
    When I publish a note saying "written where I write"
    Then the note is routed to exactly "author-write-1" and "author-write-2"
    And the note is never routed to "author-read-only"

  # ---- the p-tagged recipients' inboxes ---------------------------------

  Scenario: A p-tagged recipient adds their inbox, never their outbox
    # THE load-bearing distinction of this file. Bob's outbound relay appears
    # purely so the assertion that the resolver consumes only his inbound set
    # has something to bite on.
    Given Bob's relay list names "bob-inbox" as his read relay
    And Bob's relay list names "bob-outbox" as his write relay
    And no app relays are configured
    When I publish a note saying "hey Bob" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", and "bob-inbox"
    And the note is never routed to "bob-outbox"

  Scenario: Every p-tagged recipient contributes their own inbox
    # The fan-out is per recipient, not "the first one" and not "the union of
    # whoever the directory happened to have warm". Three addressees, three
    # inboxes, all of them.
    Given Bob's relay list names "bob-inbox" as his read relay
    And Carol's relay list names "carol-inbox" as her read relay
    And Dave's relay list names "dave-inbox" as his read relay
    And no app relays are configured
    When I publish a note saying "morning, all three of you" that p-tags Bob, Carol, and Dave
    Then the note is routed to exactly "author-write-1", "author-write-2", "bob-inbox", "carol-inbox", and "dave-inbox"

  Scenario: An unmarked relay in a recipient's list is an inbox too
    # NIP-65's marker rule, which `read_relays` states precisely: read-marked
    # entries AND unmarked entries are both read relays; only a `"write"`-
    # marked entry is excluded. An unmarked `r` tag is BOTH, so treating the
    # absence of a marker as "write only" would silently drop the most common
    # relay-list shape in the wild -- most people publish unmarked entries.
    Given Bob's relay list names "bob-unmarked" without marking it read or write
    And Bob's relay list names "bob-outbox" as his write relay
    And no app relays are configured
    When I publish a note saying "unmarked means both" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", and "bob-unmarked"
    And the note is never routed to "bob-outbox"

  # ---- how the built author, app and recipient sources combine ----------

  Scenario: All three sources land in one route, not three competing ones
    # The composition case. There is no precedence between the sources and no
    # "most specific wins" -- the resolver's answer is a set UNION, so an
    # event with an author, an operator, and an addressee reaches all of them.
    Given app relays "app-indexer-1" and "app-indexer-2" are configured
    And Bob's relay list names "bob-inbox" as his read relay
    When I publish a note saying "everyone at once" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer-1", "app-indexer-2", and "bob-inbox"
    And routing is complete

  Scenario: A relay named by two sources is one destination, not two
    # Union, not concatenation. Bob reads from a relay I also write to, which
    # is extremely ordinary; that must cost ONE lane, not two publications of
    # the same event to the same host. The lane key is `(intent_id, relay)`
    # and this is what makes the resolver's output safe to feed it.
    Given Bob's relay list names "author-write-1" as his read relay
    And no app relays are configured
    When I publish a note saying "we share a relay" that p-tags Bob
    Then the note is routed to exactly "author-write-1" and "author-write-2"
    And the note is published to "author-write-1" exactly once

  Scenario: An unrelated cached author's relays never enter the route
    # Parent provenance adds a new evidence-owned source; it does not license
    # a scan of every warm routing fact. A stranger appears only as the
    # negative control proving that a relay still needs a relationship to the
    # event being published.
    Given app relays "app-indexer-1" are configured
    And Bob's relay list names "bob-inbox" as his read relay
    And an unrelated author's relay list names "stranger-inbox" and "stranger-outbox"
    When I publish a note saying "I said nothing about relays" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "app-indexer-1", and "bob-inbox"
    And neither "stranger-inbox" nor "stranger-outbox" was ever contacted

  # ---- replies retain their observed relay context ----------------------

  Scenario: A reply returns to the relay where NMP observed its parent
    # The missing fourth contribution. The parent has exactly one verified
    # source, so this scenario does not smuggle in an unanswered choice among
    # several candidate relays. Bob's settled-empty inbox also makes the
    # parent provenance, rather than recipient routing, the only way the
    # conversation relay can enter the answer.
    Given no app relays are configured
    And a parent event by Bob is in the canonical store after NMP observed it only at "conversation-relay"
    And discovery has settled that Bob has no inbox relay
    When I publish a reply to that parent event
    Then the reply is routed to exactly "author-write-1", "author-write-2", and "conversation-relay"

  Scenario: An unverified parent relay hint does not widen Auto routing
    # The negative boundary. Signed bytes prove who authored the hint; they do
    # not prove the named relay ever held the parent. Explicit routing remains
    # the separate door for an app that intentionally chooses an exact relay.
    Given no app relays are configured
    And NMP has never observed the referenced parent at "unverified-hint-relay"
    When I publish with Auto routing a reply whose parent tag names "unverified-hint-relay" only as a relay hint
    Then the reply is routed to exactly "author-write-1" and "author-write-2"
    And "unverified-hint-relay" is never contacted
