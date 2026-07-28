Feature: Routing rewrites the queue -- half an answer is still an answer
  Routing is not a subsystem with its own delivery machinery. It is the
  operation that turns an `Auto` item in the publishing queue into destinations
  the ordinary publish machinery already knows how to serve, INCREMENTALLY:
  "perhaps all nip17 routing does is, get the two relays that should receive
  the event and literally publish the event using the exact same machinery with
  an explicit relay set of the relays that it resolved."

  So a resolver that can only half-answer neither blocks nor fails. It emits
  what it knows now and keeps the `Auto` alive for the rest -- and a resolver
  that can answer nothing at all does not consume the entry, so the next drain
  tries again.

  The destinations it emits are the intent's OWN delivery obligations, never
  child intents. That distinction carries most of the correctness: the app
  called publish once and holds ONE receipt, however many times routing
  reopens the question, and a destination already known is never re-created.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/resolution-lifecycle.md` §§1-2, 6).

  Background:
    Given I am logged in as my own account
    And an indexer relay is configured
    And my relay list names "outbox-a" as my write relay

  # ---- partial answers -------------------------------------------------

  @designed
  Scenario: What is known is delivered now; what is unknown keeps the entry alive
    # The headline behaviour. Bob's inbox is known, Carol's is not, so the note
    # goes to my own write relay and Bob's inbox immediately -- delivery does
    # not wait on the slowest unknown -- while the entry stays in the queue for
    # Carol.
    Given Bob's relay list names "bob-inbox" as his read relay
    And Carol's relay list has never been fetched
    When I publish a note saying "hello you two" mentioning Bob and Carol
    Then the note is delivered to "outbox-a"
    And the note is delivered to "bob-inbox"
    And the receipt reports it is still determining destinations
    And the receipt already reports "outbox-a" and "bob-inbox" as destinations

  @designed
  Scenario: The rest arrives later on the same receipt
    # The continuation: Carol's list lands, her inbox becomes a destination of
    # the SAME publish, and routing is then complete. No new receipt, no
    # sibling obligation, no correlation for the app to do.
    Given Bob's relay list names "bob-inbox" as his read relay
    And Carol's relay list has never been fetched
    When I publish a note saying "hello you two" mentioning Bob and Carol
    And Carol's relay list arrives naming "carol-inbox" as her read relay
    Then the note is delivered to "carol-inbox"
    And exactly one receipt exists for that publish
    And the receipt reports routing complete

  @designed
  Scenario: One party of a two-party message is reachable and the other is not
    # The owner's own worked example, generalised off NIP-17: the user sent the
    # message while totally offline, so the engine did not even know the relays
    # of both parties. It publishes the part it can -- "it does know one of the
    # relays it needs to publish to" -- and keeps the entry for the party it is
    # still missing.
    Given my own message relay is known to be "my-dm-relay"
    And Bob's message relay has never been fetched
    When I publish a message to Bob and let NMP figure out the routing
    Then the message is delivered to "my-dm-relay"
    And the receipt reports it is still determining destinations
    And the receipt names Bob as what it is still missing

  # ---- answers that are not answers yet --------------------------------

  @designed
  Scenario: An entry that can resolve nothing is not consumed
    # The other half of the owner's example: the engine "can realize it's not
    # reaching an indexer relay to retrieve the 10050 so it doesn't consume the
    # Auto entry in the publishing queue: next time the queue drains again it
    # will try again". Nothing is emitted, nothing fails, and nothing is lost.
    Given the engine is offline
    And my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then no relay is contacted
    And the receipt reports it is still determining destinations
    And the write is still held, not dropped

  @designed
  Scenario: The next drain tries again, and the one after that
    # Retrying is the entry simply still being there. Three drains with no new
    # knowledge produce three attempts and no state change; the fourth, with
    # knowledge, produces delivery.
    Given the engine is offline
    And my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    And the publishing queue drains 3 times with nothing new learned
    Then no relay is contacted
    And the receipt reports it is still determining destinations
    When my relay list arrives naming "outbox-a" as my write relay
    Then the note is delivered to "outbox-a"

  # ---- lanes, not child intents ----------------------------------------

  @designed
  Scenario: A publish that resolves in three stages still has one receipt
    # The rule stated as sharply as it can be: incremental routing changes how
    # many destinations a receipt fans out to over time; it never changes how
    # many receipts exist. Child intents would mint a receipt each and hand the
    # app N receipts for one logical publish.
    Given Bob's relay list has never been fetched
    And Carol's relay list has never been fetched
    When I publish a note saying "hello you two" mentioning Bob and Carol
    And Bob's relay list arrives naming "bob-inbox" as his read relay
    And Carol's relay list arrives naming "carol-inbox" as her read relay
    Then exactly one receipt exists for that publish
    And every per-relay outcome is reported through that one receipt
    And no second receipt was ever created for that publish

  @designed
  Scenario: Partially sent is read off one receipt, never off sibling receipts
    # What an app does with the above. "Delivered to one of two required
    # relays" is a per-destination fact on a single receipt -- the per-relay
    # ack tracking that already ships -- not two receipts to correlate.
    Given Bob's relay list names "bob-inbox" as his read relay
    And relay "bob-inbox" rejects every event
    When I publish a note saying "hello Bob" mentioning Bob
    Then the receipt reports the note acked by "outbox-a"
    And the receipt reports the note rejected by "bob-inbox"
    And exactly one receipt exists for that publish

  @designed
  Scenario: Re-resolution that reveals a known relay creates nothing
    # Re-spawn suppression, which falls out of destinations being keyed by
    # (intent, relay). A resolver reporting a relay the intent already has --
    # pending, in flight, or acked -- collides with what exists and mints
    # nothing.
    Given Bob's relay list names "outbox-a" as his read relay
    When I publish a note saying "hello Bob" mentioning Bob
    Then the receipt reports exactly one destination
    And "outbox-a" was offered the note exactly once
