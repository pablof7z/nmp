Feature: A write made before we knew anything waits, it does not die
  Publishing before the author's relay list has ever been fetched must PARK and
  deliver when the list arrives. Today it does the opposite, and this is the
  defect the whole lifecycle exists to fix.

  Verified on master: `AuthorOutbox` errors whenever the directory knows no
  write relays for the author (`crates/nmp/src/core/write.rs:2599-2600`), and a
  routing error at signature time removes the pending write, emits
  `Failed`, and returns (`:2229-2243`). Compose the two and publishing anything
  on a first run, a cold start, or while offline kills the write PERMANENTLY.
  The event was signed, journalled and durable; the app did everything right;
  the directory was merely young.

  The tell that it is a defect rather than a policy is the asymmetry: boot
  recovery treats the very same routing error as an empty answer and moves on
  (`:876`), so a crash-survivor outlives the exact condition that kills a fresh
  write. Under this design "no relays known yet" stops being an error at all --
  it is an `Auto` with unknowns, which is the NORMAL INITIAL STATE of the queue
  rewriter.

  Every scenario here is an acceptance criterion for unbuilt work
  (`docs/internals/routing/resolution-lifecycle.md` §8).

  Background:
    Given I am logged in as my own account
    And an indexer relay is configured

  Scenario: The very first publish of a fresh install parks and then delivers
    # The headline case. Nothing has ever been fetched; the user hits send.
    # Today this write is dropped and never mentioned again.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the publish is accepted
    And the receipt does not report a failure
    And the receipt reports it is still determining destinations
    When my relay list arrives naming "outbox-a" as my write relay
    Then the note is delivered to "outbox-a"

  Scenario: The park names what it is waiting for
    # A park nobody can see is indistinguishable from data loss, so the detail
    # is the difference between "stuck" and "stuck because X" -- and X is the
    # only thing an app or user can act on.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    Then the receipt says it has no relay list for me yet

  @designed
  Scenario: A young directory treats a fresh write and a recovered one alike
    # The asymmetry, closed. Two writes in the same condition -- one signed
    # just now, one recovered from a store -- must reach the same state. Today
    # the first one dies and the second survives, and nothing about their
    # situations differs.
    Given my relay list has never been fetched
    And a note saying "from before" is recovered from the durable store with its routing unresolved
    When I publish a note saying "from now" and let NMP figure out the routing
    Then the receipt for "from now" reports it is still determining destinations
    And the receipt for "from before" reports it is still determining destinations
    When my relay list arrives naming "outbox-a" as my write relay
    Then the note saying "from before" is delivered to "outbox-a"
    And the note saying "from now" is delivered to "outbox-a"

  @designed
  Scenario: The park survives a restart with its reason intact
    # Parks are durable, retained, and replayed on reattachment -- the routing
    # sibling of the signer park that already ships. A route parked for a month
    # is still visible, with its reason, a month later.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    And the process stops with the note undelivered
    And I reconstruct the engine from the same durable store
    Then the same receipt can be reattached by its stable id
    And the receipt reports it is still determining destinations
    And the receipt says it has no relay list for me yet

  @designed
  Scenario: Nothing gives up on its own
    # There is no TTL on a parked route, no heuristic that decides a relay list
    # will "never" arrive. NMP can no more prove that than it can prove
    # wss://non-existent.com will never resolve, and a durable queue that drops
    # obligations on a guess is worse than one that holds them visibly.
    Given my relay list has never been fetched
    When I publish a note saying "hello" and let NMP figure out the routing
    And 30 days pass with nothing learned
    Then the write is still held, not dropped
    And the receipt reports it is still determining destinations
    And the receipt says it has no relay list for me yet
    When my relay list arrives naming "outbox-a" as my write relay
    Then the note is delivered to "outbox-a"

  @designed
  Scenario: A cold start does not silently under-route
    # The failure mode the park replaces is not only "the write dies" -- it is
    # also the tempting alternative, publishing to whatever happens to be known
    # and calling it done. A write that goes to two of its five destinations
    # and reports success is worse than one that waits, and for a message it is
    # a privacy failure rather than a delivery one.
    Given my relay list has never been fetched
    And Bob's relay list names "bob-inbox" as his read relay
    When I publish a note saying "hello Bob" mentioning Bob
    Then the note is delivered to "bob-inbox"
    And the receipt reports it is still determining destinations
    And the receipt never reported routing complete
