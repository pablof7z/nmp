Feature: Fallback relays top up a recipient nobody else can reach
  The read path has a third operator-configured set beyond the app relays:
  the fallback relays (`RoutingFacts::operator_fallback_relays`), applied per
  author only when that author's own coverage falls under the 2-relay minimum
  AND no app relay is configured -- app relays suppress fallback entirely.

  Pablo ruled that the write side adopts the same set under the same rule, and
  it does.

  The failure this closes is concrete, not theoretical. You reply to someone
  whose kind:10002 names exactly one relay. Without a top-up the reply goes to
  that single relay and nowhere else, so if it is down the person you are
  replying to never sees your answer. Reads already faced this exact question
  about this exact author and answered it; a write that cannot reach its
  addressee is the worse half of the problem, not the lesser one.

  The counter-argument, considered and rejected: a write fans out to every
  known write relay of its author and needs no coverage-solving there, which
  reads as though the 2-relay minimum has no write-side analogue at all. It
  has one -- it is about the RECIPIENT's coverage rather than the author's
  fan-out. Fanning out to every known write relay and topping up a recipient
  below coverage are independent questions, and adopting the second does not
  weaken the first. That distinction is asserted in both directions below,
  because it is the part of this rule most likely to be implemented too
  broadly.

  Background:
    Given I am logged in as my own account
    And my relay list names "author-write-1" and "author-write-2" as my write relays
    And fallback relays "fallback-1" and "fallback-2" are configured

  # ---- the motivating case ----------------------------------------------

  Scenario: Replying to someone whose relay list names exactly one relay
    # The case the ruling was made on. Bob published a kind:10002 with a
    # single entry. One relay is one point of failure for the entire reply,
    # and Bob is the one person who must receive it. The fallbacks top HIM up;
    # they are not a general widening of the route.
    Given Bob's relay list names "bob-only-inbox" as his one read relay
    And no app relays are configured
    When I publish a note saying "answering you, Bob" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "bob-only-inbox", "fallback-1", and "fallback-2"
    And routing is complete

  Scenario: A recipient with no reachable inbox at all is topped up the same way
    # Zero is below two. A recipient whose relay list is settled as absent
    # contributes no inbox of its own, and the fallbacks are the only chance
    # the note has of reaching them. Note what this does NOT do: it does not
    # keep the routing open. Absence is settled knowledge, so this completes
    # (outbox-recipients-and-settlement.feature owns that half).
    Given the indexers have finished their stored events without a relay list for Bob
    And no app relays are configured
    When I publish a note saying "somewhere, I hope" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "fallback-1", and "fallback-2"
    And routing is complete

  # ---- suppression ------------------------------------------------------

  Scenario: App relays suppress fallback entirely
    # The rule as the read path states it, transplanted verbatim. An operator
    # who configured app relays has already answered "where should things go
    # when a user's own list is thin" -- adding a second, generic set on top
    # would send the reply somewhere the operator never chose. Note that the
    # app relays do NOT themselves count as coverage for Bob (they are "never
    # counted toward the 2-relay-min"); they suppress the top-up without
    # satisfying it, and that is the intended, slightly counter-intuitive
    # shape.
    Given Bob's relay list names "bob-only-inbox" as his one read relay
    And app relays "app-indexer" are configured
    When I publish a note saying "answering you, Bob" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "bob-only-inbox", and "app-indexer"
    And the note is never routed to "fallback-1"
    And the note is never routed to "fallback-2"

  Scenario: One configured app relay is enough to suppress, however thin it is
    # Suppression is on the PRESENCE of an app relay set, not on its size or
    # on whether it restores coverage. A single app relay leaves Bob served by
    # two hosts neither of which is a fallback, and that is the operator's
    # call to have made.
    Given the indexers have finished their stored events without a relay list for Bob
    And app relays "app-indexer" are configured
    When I publish a note saying "somewhere, I hope" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", and "app-indexer"
    And the note is never routed to "fallback-1"

  # ---- adequate coverage ------------------------------------------------

  Scenario: A recipient already at coverage gets no fallback
    # Two is the minimum, and two is enough. Bob has two inboxes, so the
    # top-up has nothing to fix and adding the fallbacks would be a pure
    # widening of the route -- more relays holding a note than anyone asked
    # for, on every reply, forever.
    Given Bob's relay list names "bob-inbox-1" and "bob-inbox-2" as his read relays
    And no app relays are configured
    When I publish a note saying "you are well covered" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", "bob-inbox-1", and "bob-inbox-2"
    And the note is never routed to "fallback-1"
    And the note is never routed to "fallback-2"

  Scenario: The decision is per recipient, and one short recipient is enough to arm it
    # "Per author" is only observable through a pair like this. Carol alone
    # arms nothing; Bob alone arms the top-up; the two of them together still
    # arm it, because Carol being well covered says nothing about Bob. An
    # implementation that averaged coverage across the recipient set, or that
    # asked "does this event have enough relays" rather than "does this
    # PERSON", would pass the first two scenarios of this file and fail here.
    Given Bob's relay list names "bob-only-inbox" as his one read relay
    And Carol's relay list names "carol-inbox-1" and "carol-inbox-2" as her read relays
    And no app relays are configured
    When I publish a note saying "one of you is thin" that p-tags Bob and Carol
    Then the note is routed to exactly "author-write-1", "author-write-2", "bob-only-inbox", "carol-inbox-1", "carol-inbox-2", "fallback-1", and "fallback-2"

  Scenario: An amply covered recipient alone never arms the top-up
    # The control for the scenario above, and the guard against buying it by
    # applying fallback whenever any recipient exists at all.
    Given Carol's relay list names "carol-inbox-1" and "carol-inbox-2" as her read relays
    And no app relays are configured
    When I publish a note saying "you are fine on your own" that p-tags Carol
    Then the note is routed to exactly "author-write-1", "author-write-2", "carol-inbox-1", and "carol-inbox-2"
    And the note is never routed to "fallback-1"

  # ---- what the top-up must NOT become ----------------------------------

  Scenario: The author's own thin fan-out is not a coverage problem
    # The line drawn in this feature's preamble, asserted. A write already
    # fans out to EVERY write relay the author has; there is no per-author
    # solving to do on that side, and one write relay of my own is a fact
    # about where I publish rather than a coverage deficit to repair. The
    # 2-relay minimum on the write side is about reaching the ADDRESSEE.
    # Widening this to the author would put every note anyone publishes from a
    # single-relay setup onto the operator's fallbacks, which is a different
    # policy nobody ruled on.
    Given my relay list names only "author-write-1" as my write relay
    And no app relays are configured
    When I publish a note saying "one relay is where I publish"
    Then the note is routed to exactly "author-write-1"
    And the note is never routed to "fallback-1"
    And the note is never routed to "fallback-2"

  Scenario: No fallback relays configured and a thin recipient is not an error
    # Nothing about the top-up is required for routing to succeed. With no
    # fallbacks configured the route is simply what the three sources yielded,
    # and it completes. A resolver that treated "below coverage and nothing to
    # top up with" as a failure would break every app that never configured
    # the set.
    Given no fallback relays are configured
    And no app relays are configured
    And Bob's relay list names "bob-only-inbox" as his one read relay
    When I publish a note saying "answering you, Bob" that p-tags Bob
    Then the note is routed to exactly "author-write-1", "author-write-2", and "bob-only-inbox"
    And routing is complete
