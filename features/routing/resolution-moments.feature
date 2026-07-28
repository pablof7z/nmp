Feature: Routing is re-decided at every send opportunity, never once
  A routing value stores HOW TO DECIDE, not WHERE TO SEND. A relay set is a
  moment's answer; the strategy is the durable thing. So at every send
  opportunity -- the first attempt, after a crash, when the app comes back
  online and the unpublished queue starts draining, when a write that parked
  for a signer finally gets one -- the strategy is executed fresh against
  whatever the engine knows AT THAT MOMENT.

  This matters in exactly the cases that matter most. On a cold start the
  user's own relay list has not been fetched yet. A write that parks for hours
  outlives the relay list it would have been resolved against. A snapshot
  taken at compose time is wrong in both.

  Half of this already ships: master resolves at signature time and again at
  boot recovery, both reading the directory at that moment, and boot already
  diffs the fresh answer against everything the intent has ever durably
  resolved to. What is unbuilt is the other two moments and what happens when
  resolution comes up short (`docs/internals/routing/resolution-lifecycle.md`
  §5).

  Background:
    Given I am logged in as my own account

  @designed
  Scenario: The first attempt resolves against what is known then
    # Moment one, and the only one that exists in most people's mental model.
    Given my relay list names "outbox-a" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the note is delivered to "outbox-a"

  @designed
  Scenario: A restart re-decides against the new process's knowledge
    # Moment two. The strategy, not the answer, is what survived the crash --
    # so a relay learned while the process was down gets a delivery, and the
    # app holds the same receipt it always held. This is the one moment master
    # already gets right, and it is the model for the rest.
    Given my relay list names "outbox-a" as my write relay
    And relay "outbox-a" never acknowledges anything
    When I publish a note saying "hello" and let NMP figure out the routing
    And the process stops with the note undelivered
    And my relay list changes to name "outbox-a" and "outbox-c" as my write relays
    And I reconstruct the engine from the same durable store
    Then the note is delivered to "outbox-c"
    And the same receipt can be reattached by its stable id
    And exactly one receipt exists for that publish

  @designed
  Scenario: A draining offline queue re-decides rather than replaying a snapshot
    # Moment three, in the form the owner described it: "when the app comes
    # back online and the unpublished event queue starts getting drained
    # calculations according to whatever routing has been decided is
    # performed". The note was accepted while the engine knew nothing; the
    # drain is when the strategy actually runs.
    Given the engine is offline
    And my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then no relay is contacted
    When the engine comes back online
    And my relay list arrives naming "outbox-a" as my write relay
    Then the note is delivered to "outbox-a"

  @designed
  Scenario: A write parked for a signer is routed against the list as it stands at signing
    # The long-park case. A NIP-46 signer is offline for a week; meanwhile the
    # user moves relays. When the signature finally arrives, resolution runs
    # then -- against the CURRENT list -- so the note goes where the user
    # writes now, not where they wrote when they hit send. A compose-time
    # snapshot would deliver to a relay the user has abandoned.
    Given a NIP-46 signer is registered for the current pubkey but is offline
    And my relay list names "outbox-old" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports awaiting that pubkey's signer
    And no relay is contacted
    When my relay list changes to name "outbox-new" as my write relay
    And the matching signer provider reattaches
    Then the note is delivered to "outbox-new"
    And "outbox-old" was never contacted

  @designed
  Scenario: A relay list arriving for any reason is consulted without any special wiring
    # Moment four's safety net: intents whose routing is not complete are
    # re-resolved on the ordinary engine tick, so a kind:10002 ingested for a
    # completely unrelated reason -- someone opened a profile, a feed
    # hydrated -- is picked up with no wiring between the read path and the
    # write path at all.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt reports it is still determining destinations
    When I open Alice's profile, and my own relay list arrives alongside it naming "outbox-a"
    Then the note is delivered to "outbox-a"

  @designed
  Scenario: Settling what a parked write was waiting for wakes it in the same turn
    # Moment four proper: latency, where the tick is correctness. The parked
    # write declared what it needed; the need settles; the write resolves
    # within the same ingestion turn rather than waiting for the next tick.
    Given my relay list has never been fetched
    And an indexer relay is configured
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the indexer is asked for my relay list
    When the indexer delivers my relay list naming "outbox-a" as my write relay
    Then the note is delivered to "outbox-a" without waiting for another tick

  @designed
  Scenario: Which moment fired is never observable
    # Because resolution diffs against what the intent has already durably
    # resolved to, running "too often" costs a directory read and an empty
    # diff. This scenario pins that the same sequence of knowledge produces
    # the same deliveries whether it arrives during a tick, during a drain, or
    # at boot -- correctness may never depend on which moment noticed.
    Given my relay list names "outbox-a" as my write relay
    When I publish a note saying "hello" and let NMP figure out the routing
    And the engine ticks 20 times
    Then the note is delivered to "outbox-a"
    And "outbox-a" was offered the note exactly once

  @designed
  Scenario: An explicit route is the fixed point of the same loop
    # Explicit degenerates exactly as it should: resolved once, verbatim, with
    # nothing left to learn, so every later moment is a no-op. The strategy
    # model costs the simple case nothing.
    When I publish a note saying "hello" to exactly "chosen-relay"
    And the process stops with the note undelivered
    And my relay list changes to name "outbox-c" as a write relay
    And I reconstruct the engine from the same durable store
    Then the note is delivered to "chosen-relay"
    And "outbox-c" was never contacted
