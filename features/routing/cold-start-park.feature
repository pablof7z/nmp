Feature: A write made before we knew anything waits; a write with nowhere to go is told so
  Publishing before the author's relay list has been fetched must PARK and
  deliver when the list arrives. Publishing when the lookup has FINISHED and
  found nothing must terminate and say so. The two look identical from the
  outside -- no relays, nothing sent -- and telling them apart is the whole
  job of this file.

  The old defect was that both died. `AuthorOutbox` errored whenever the
  directory knew no write relays for the author, and a routing error at
  signature time removed the pending write, emitted a failure and returned.
  Compose the two and publishing anything on a first run, a cold start, or
  while offline killed the write PERMANENTLY. The event was signed, journalled
  and durable; the app did everything right; the directory was merely young.

  The overcorrection would be that both park. That is worse for the second
  case, not better: a user with no relays configured watches a message sit
  forever in a state the app renders as "sending", and nothing will ever
  change because there is nothing left to learn. **A write parked forever
  while the user believes it was sent is the defect this file now guards
  against**, on equal footing with the write that died.

  `RouteAnswer.complete` is the bit that separates them, and it is a statement
  about KNOWLEDGE EXHAUSTION, never about delivery:

  - `complete: false` with no relays -- still looking. Park, indefinitely,
    with no cap of any kind. Nothing accumulates in that state, so a deadline
    over it would convert ignorance into a verdict.
  - `complete: true` with no relays -- finished looking, and there is nowhere
    to publish. Terminal `NoDestination`, immediately, and readable in the
    queue so the app can tell someone "you have no relays configured" instead
    of showing a spinner that never resolves.

  Owner ruling (2026-08-04): *"if the app has no app relay and no indexer then
  the user's own relay list has no where to go -- so yes, it should also
  fail."* The same answer covers an indexer that DID answer and found no relay
  list: the lookup finished, so the write terminates rather than waiting on a
  question already answered.

  Background:
    Given I am logged in as my own account

  Rule: Not looked up yet parks, and delivers when the answer arrives

    # nmp:id=ROUTING-COLDSTART-001
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Terminate a write whose relay-list lookup has not settled; a user who was merely offline on first run loses a message NMP never proved undeliverable.
    Scenario: The very first publish of a fresh install parks and then delivers
      # The headline case. An indexer exists and has not answered yet, so
      # nothing has been learned and the write waits for it.
      Given an indexer relay is configured
      And my relay list lookup has not settled
      When I publish a note saying "hello" and let NMP figure out the routing
      Then the publish is accepted
      And the receipt does not report a failure
      And the receipt reports it is still determining destinations
      When my relay list arrives naming "outbox-a" as my write relay
      Then the note is delivered to "outbox-a"

    # nmp:id=ROUTING-COLDSTART-002
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Let recovery and a fresh publish reach different states from the same unsettled directory; a crash survivor would outlive the exact condition that ends a fresh write.
    Scenario: A young directory treats a fresh write and a recovered one alike
      Given an indexer relay is configured
      And my relay list lookup has not settled
      And a note saying "from before" is recovered from the durable store with its routing unresolved
      When I publish a note saying "from now" and let NMP figure out the routing
      Then the receipt for "from now" reports it is still determining destinations
      And the receipt for "from before" reports it is still determining destinations
      When my relay list arrives naming "outbox-a" as my write relay
      Then the note saying "from before" is delivered to "outbox-a"
      And the note saying "from now" is delivered to "outbox-a"

    # nmp:id=ROUTING-COLDSTART-003
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Drop the park across a restart; an app that reattaches to a persisted receipt id is told nothing, which is indistinguishable from data loss.
    Scenario: The park survives a restart
      Given an indexer relay is configured
      And my relay list lookup has not settled
      When I publish a note saying "hello" and let NMP figure out the routing
      And the process stops with the note undelivered
      And I reconstruct the engine from the same durable store
      Then the same receipt can be reattached by its stable id
      And the receipt reports it is still determining destinations

    # nmp:id=ROUTING-COLDSTART-004
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Expire an unsettled park after any elapsed time; NMP would be deciding a relay list will never arrive, which it cannot prove.
    Scenario: Nothing gives up on an unsettled lookup
      Given an indexer relay is configured
      And my relay list lookup has not settled
      When I publish a note saying "hello" and let NMP figure out the routing
      And 30 days pass with nothing learned
      Then the write is still held, not dropped
      And the receipt reports it is still determining destinations
      When my relay list arrives naming "outbox-a" as my write relay
      Then the note is delivered to "outbox-a"

  Rule: Looked up and found nothing is terminal, and says why

    # nmp:id=ROUTING-COLDSTART-005
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Park a write whose relay-list lookup finished and found nothing; the user watches a message sit in "sending" forever while the app has everything it needs to tell them they have no relays configured.
    Scenario: A settled lookup that found no relay list ends the write
      # The scenario this file used to get backwards. The indexer answered.
      # There is no relay list, no app relay and no fallback, so there is
      # nowhere to publish -- and that is a FACT the app can act on, not a
      # question still open.
      Given an indexer relay is configured
      And the indexers have settled that I have no relay list
      When I publish a note saying "hello" and let NMP figure out the routing
      Then the publish is accepted
      And the receipt reports a closed destination set naming no relays
      And the receipt reports the write as having no destination
      And the write is never reported as settled

    # nmp:id=ROUTING-COLDSTART-006
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Park a write when nothing can ever resolve it; with no app relay and no indexer there is no source that could change the answer, so waiting is waiting on nobody.
    Scenario: No app relay and no indexer is nowhere to go
      # The owner's own case. Nothing is configured that could ever produce a
      # destination, so the answer is already final at the first attempt.
      Given no app relay is configured
      And no indexer relay is configured
      When I publish a note saying "hello" and let NMP figure out the routing
      Then the publish is accepted
      And the receipt reports the write as having no destination

    # nmp:id=ROUTING-COLDSTART-007
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Leave a no-destination write's open work behind; the entry cannot be removed (removal refuses an open intent), cannot be cancelled once signed, and is replayed on every boot -- a leak on the FIRST publish of a fresh install.
    Scenario: A write with nowhere to go is readable in the queue and removable
      # The user-facing point. "Nowhere to publish" is only useful if the app
      # can still find the write and say so, and only honest if the app can
      # then get rid of it.
      Given an indexer relay is configured
      And the indexers have settled that I have no relay list
      When I publish a note saying "hello" and let NMP figure out the routing
      And I enumerate my publish queue
      Then the entry reports the write as having no destination
      When I remove that entry from my publish queue
      Then my publish queue no longer holds that entry

    # nmp:id=ROUTING-COLDSTART-008
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Report the same destination fact before and after the lookup settles; the two states have opposite correct app behaviour and collapsing them is the defect this whole file is about.
    Scenario: Settling turns a park into a terminal answer
      Given an indexer relay is configured
      And my relay list lookup has not settled
      When I publish a note saying "hello" and let NMP figure out the routing
      Then the receipt reports it is still determining destinations
      When the indexers settle that I have no relay list
      Then the receipt reports a closed destination set naming no relays
      And the receipt reports the write as having no destination

  Rule: A partial answer is never rounded up into a complete one

    # nmp:id=ROUTING-COLDSTART-009
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Report routing complete while a recipient is still unresolved; a message delivered to two of its five destinations and called sent is a privacy failure, not a delivery one.
    Scenario: A cold start does not silently under-route
      Given an indexer relay is configured
      And my relay list lookup has not settled
      And Bob's relay list names "bob-inbox" as his read relay
      When I publish a note saying "hello Bob" mentioning Bob
      Then the note is delivered to "bob-inbox"
      And the receipt reports it is still determining destinations
      And the receipt never reported routing complete
