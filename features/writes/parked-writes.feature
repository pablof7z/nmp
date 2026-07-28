Feature: A write that cannot move is parked in the open, with its reason
  Acceptance cannot know. A DM composed while the app was offline, for a
  recipient whose relay list turns out never to have been published, is a
  well-formed obligation that only the world can refuse -- later,
  asynchronously, possibly never with a definitive answer.

  > there's no way to know that at acceptance time in the same way that we
  > can't know if the user says "when you go online publish this to
  > wss://non-existent.com"

  Since failure after acceptance is therefore normal rather than exceptional,
  the write plane's answer is to park, not to fail: the obligation stays
  accepted, stays visible, and carries the reason it cannot move. A park that
  says only "stuck" is barely better than losing it -- the difference between
  "stuck" and "stuck because Bob has no DM relay list" is the whole of what
  an app or a person can act on.

  The reason has to outlive the process. An app that restarts, reattaches to
  a receipt it persisted the id of, and is told nothing has learned nothing;
  a park nobody can see again is indistinguishable from data loss. So the
  reason is retained on the receipt and replayed verbatim on reattachment,
  a month later if that is how long it takes.

  And nothing expires. There is no TTL on a parked route, no cap that
  terminally fails an unreachable relay into oblivion, no heuristic that
  decides a recipient will "never" publish a relay list. NMP can no more
  prove wss://non-existent.example will never resolve than it can prove a DM
  relay list will never appear -- both are open-ended facts about the world,
  and a durable queue that quietly drops obligations on a guess is worse than
  one that holds them where they can be seen. Explicit cancellation is the
  only way out.

  Background:
    Given only the indexer relay "wss://indexer.example" is configured
    And my relay list names "wss://relay.mine.example" as my write relay
    And I am logged in as my own account

  # ---- parking, and what a park says ------------------------------------

  @designed @ledger-9
  Scenario: A write with nowhere to go is parked, not failed
    Given the indexers have settled that Bob has no DM relay list
    When I publish a direct message to Bob
    Then the receipt first reports only accepted -- never sent
    And the receipt reports the write awaiting a route
    And the reason names Bob's missing DM relay list
    And the write is never reported as failed

  @designed @ledger-9
  Scenario: Every parked write names what it is waiting for
    # The contract on the detail. A park with an empty reason is the failure
    # mode this whole feature exists to prevent: an app can render it, and a
    # person can read it, and neither of them learns anything.
    Given the indexers have settled that Bob has no DM relay list
    When I publish a direct message to Bob
    Then the receipt reports the write awaiting a route
    And the reason is not empty
    And the reason names the recipient it cannot resolve

  @designed @ledger-9
  Scenario: Publishing before the first relay list has been fetched parks
    # The case an app hits on its very first run: publish immediately, before
    # any relay list has been fetched. Routing cannot answer yet -- which is a
    # reason to wait, not a reason to destroy the obligation.
    Given no relay list has been fetched yet
    When I publish a note saying "first post"
    Then the receipt reports the write awaiting a route
    And the reason names the relay list that has not been fetched
    And the write is never reported as failed
    And the intent is still durably held

  # ---- the reason has to outlive the process ----------------------------

  @designed @ledger-9 @ledger-16
  Scenario: The reason is replayed when an app reattaches after a restart
    Given the indexers have settled that Bob has no DM relay list
    And I published a direct message to Bob and it was parked awaiting a route
    When the process stops immediately
    And I reconstruct the engine from the same durable store
    And I reattach to the receipt by its stable id
    Then the receipt reports the write awaiting a route
    And the reason is the same reason it was parked with

  @designed @ledger-16
  Scenario: A park a month old is still a park, with the same reason
    # Age does not soften into ambiguity. Whatever the reason said on day one
    # it still says on day thirty, because it is the recorded reason and not a
    # re-derivation against knowledge that has moved on since.
    Given a direct message to Bob has been parked awaiting a route for 30 days
    When I reattach to the receipt by its stable id
    Then the receipt reports the write awaiting a route
    And the reason names Bob's missing DM relay list
    And the receipt reports how long it has been parked

  # ---- a park is not terminal -------------------------------------------

  @designed @ledger-9
  Scenario: A parked route resumes on its own when the knowledge arrives
    # Park means waiting, and waiting means it can end. Nothing about
    # unparking requires the app to notice, retry, or re-publish anything.
    Given nothing is known yet about Bob's DM relay list
    And I published a direct message to Bob and it was parked awaiting a route
    When Bob's DM relay list arrives naming "wss://inbox.bob.example"
    Then the write is routed to "wss://inbox.bob.example"
    And the receipt no longer reports the write awaiting a route
    And the same write is delivered -- not a second copy of it

  @designed @ledger-16
  Scenario: A route parked across a restart resumes after the restart
    Given nothing is known yet about Bob's DM relay list
    And I published a direct message to Bob and it was parked awaiting a route
    When the process stops immediately
    And I reconstruct the engine from the same durable store
    And Bob's DM relay list arrives naming "wss://inbox.bob.example"
    Then the write is routed to "wss://inbox.bob.example"

  # ---- nothing auto-abandons --------------------------------------------

  @designed @ledger-16
  Scenario: No amount of time abandons a parked write
    Given the indexers have settled that Bob has no DM relay list
    And I published a direct message to Bob and it was parked awaiting a route
    When 90 days pass
    Then the receipt still reports the write awaiting a route
    And the reason names Bob's missing DM relay list
    And the intent is still durably held
    And nothing abandoned the write on NMP's own initiative

  @designed @ledger-16
  Scenario: An unreachable relay is retried forever rather than forgotten
    # The other half of the same rule, on the delivery side. Retries back off
    # -- they must -- but backing off is not giving up, and the obligation is
    # never deleted because a counter ran out.
    Given I am told to publish a note to exactly "wss://non-existent.example"
    When I publish that note
    And 90 days pass
    Then the write was routed to "wss://non-existent.example"
    And the receipt still reports "wss://non-existent.example" as undelivered
    And the intent is still durably held

  @designed @ledger-16
  Scenario: Cancellation is the only way a write leaves
    # The one abandonment door, and it is the app's. Someone decided; that is
    # the difference between this and a timeout.
    Given the indexers have settled that Bob has no DM relay list
    And I published a direct message to Bob and it was parked awaiting a route
    When I cancel that write
    Then the receipt reports the write cancelled
    And the reason it was parked with is still readable
    And the intent is no longer held for delivery
