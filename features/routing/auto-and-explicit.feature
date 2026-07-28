Feature: An app says "figure it out" or "these exact relays", and nothing else
  The whole app-facing routing surface is two words. `Auto` means "figure it
  out how to route whatever I'm publishing"; `Explicit` means "use these exact
  relays and that is that no matter what else happens". There is no third
  word -- no "outbox", no "nip17", no "nip29", no "draft" -- because which
  strategy claims a kind is NMP's own business, decided at send time, never
  spelled by the app.

  `Explicit` is a general capability, not a NIP-29 concession: an app offering
  "publish this event to relay: [user input]", a wiki crate publishing to the
  user's preferred wiki relays, a DM crate publishing to two parties' DM
  relays, and a user right-clicking someone else's note to archive it are all
  the same primitive. It executes verbatim, it never widens, and an empty one
  is refused before anything durable exists.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/auto-and-explicit.md`). Today the app surface has
  exactly one route, `AuthorOutbox`, and none of this is reachable.

  Background:
    Given I am logged in as my own account
    And my relay list names "outbox-a" and "outbox-b" as my write relays
    And no app relays are configured

  # ---- the two words ---------------------------------------------------

  @designed
  Scenario: An ordinary note names no relays at all
    # The default case, and the one the app should never have to think about.
    # The app hands over a note and a routing of "figure it out"; it passes no
    # relay list, names no strategy, and gets back one receipt.
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the note is delivered to "outbox-a" and "outbox-b"
    And the app named no relay anywhere in that publish
    And exactly one receipt exists for that publish

  @designed
  Scenario: An app naming exact relays gets exactly those relays
    # "use these exact relays and that is that no matter what else happens".
    # My own write relays are known and still receive nothing: an Explicit
    # route never consults the directory, so there is nothing for the
    # directory to contribute.
    When I publish a note saying "for the archive" to exactly "chosen-relay"
    Then the note is delivered to "chosen-relay"
    And "outbox-a" was never contacted
    And "outbox-b" was never contacted

  @designed
  Scenario: A relay learned after acceptance is never added to an explicit route
    # There is no widen path anywhere: no operation adds a relay to an
    # accepted Explicit route, which is ledger #6's `NarrowOnly` discipline
    # carried over structurally rather than by convention. Learning more must
    # therefore change nothing here.
    Given the engine is offline
    When I publish a note saying "for the archive" to exactly "chosen-relay"
    And my relay list changes to name "outbox-c" as a write relay
    And the engine comes back online
    Then the note is delivered to "chosen-relay"
    And "outbox-c" was never contacted
    And the receipt reports exactly one destination

  @designed
  Scenario Outline: The same two words serve every reason to pick a relay
    # The reversal, spelled out per named case so nobody rebuilds the ban one
    # consumer at a time. A user-typed relay, a wiki crate's preferred kind, a
    # DM crate's pair of parties, a group host: all of them are one app or one
    # crate saying "these exact relays". None of them needs a grammar variant,
    # an authority newtype, or a capability check.
    When "<publisher>" publishes an event to exactly <relays>
    Then the event is delivered to <relays>
    And no relay outside <relays> was ever contacted
    And the routing it used is the same one any app can express

    Examples:
      | publisher       | relays                                |
      | the app itself  | "user-typed-relay"                    |
      | nmp-wiki        | "wiki-relay-a" and "wiki-relay-b"     |
      | nmp-nip17       | "bob-dm-relay" and "my-dm-relay"      |
      | nmp-nip29       | "group-host"                          |

  # ---- routing is independent of authorship ----------------------------

  @designed
  Scenario: Republishing someone else's signed event to my own archive relay
    # The proof that routing and authorship are separate axes: the user sees a
    # cool event from someone they follow, right-clicks, and publishes that
    # exact signed event, as-is, to their own personal archive relay. The
    # route is mine, the signature is Alice's, and no fact about either was
    # consumed by the other.
    Given Alice has posted a note saying "worth keeping" signed by Alice
    When I publish Alice's signed note unchanged to exactly "my-archive-relay"
    Then "my-archive-relay" received the note with Alice's signature untouched
    And the note's event id is the one Alice signed
    And no signer was asked for anything
    And nothing identifying me appears anywhere in the payload
    And "outbox-a" was never contacted

  @designed
  Scenario: My own relay list has no bearing on where someone else's event goes
    # The stronger form of the same case. My directory is fully populated, my
    # write relays are known and healthy, and none of that is an input to a
    # route I chose explicitly for an event I did not sign.
    Given Alice has posted a note saying "worth keeping" signed by Alice
    And Alice's relay list names "alice-relay" as her write relay
    When I publish Alice's signed note unchanged to exactly "my-archive-relay"
    Then the note is delivered to "my-archive-relay"
    And "alice-relay" was never contacted
    And "outbox-a" was never contacted
    And "outbox-b" was never contacted

  # ---- the empty route -------------------------------------------------

  @designed
  Scenario: An explicit route with no relays is rejected immediately
    # The owner's ruling, verbatim: "reject it immediately". This is stricter
    # than master, and deliberately so -- today an empty `PrivateNarrow` is
    # accepted and then fails closed at resolution, which made emptiness a
    # SENTENCE ("I resolved this and there is nowhere safe to send it") rather
    # than a mistake. That sentence moves to the resolver's refusal reason and
    # to the route preview, both of which can explain themselves where an
    # empty `Vec` cannot. So the empty route stops being expressible at all.
    When I publish a note saying "nowhere" to exactly no relays
    Then the publish is refused before anything is accepted
    And no receipt is created
    And nothing is written to the journal
    And no relay is contacted

  @designed
  Scenario: The refusal happens before a signature is ever requested
    # Refused "at the door" means before acceptance, not after signing and
    # before delivery. Nothing durable, and nothing asked of the signer.
    Given a signer is registered for the current pubkey
    When I publish a note saying "nowhere" to exactly no relays
    Then the publish is refused before anything is accepted
    And no signer was asked for anything

  @designed
  Scenario: An unreachable relay is accepted, because acceptance cannot know
    # The deliberate asymmetry with the scenario above, and the reason it is
    # safe. Emptiness is a property of the request, knowable at the door.
    # Reachability is a property of the world: "we can't know if the user says
    # 'when you go online publish this to wss://non-existent.com'". So this one
    # is accepted, routed instantly, and fails visibly per relay -- never
    # refused, and never silently dropped.
    When I publish a note saying "hello" to exactly "wss://non-existent.com"
    Then the publish is accepted
    And the receipt reports routing complete
    And the receipt reports the failure to reach "wss://non-existent.com"
    And the write is still held, not dropped
