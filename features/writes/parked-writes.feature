Feature: A write that cannot move is parked in the open, and a write that has been refused enough times stops
  Acceptance cannot know. A DM composed while the app was offline, for a
  recipient whose relay list has not been fetched yet, is a well-formed
  obligation that only the world can answer -- later, asynchronously.

  > there's no way to know that at acceptance time in the same way that we
  > can't know if the user says "when you go online publish this to
  > wss://non-existent.com"

  The governing rule is one sentence:

  > **Nothing in the publish queue terminates on a clock. Everything
  > terminates on a fact.**

  That is a rule about the KIND of evidence, not a ban on ending. An attempt
  ceiling satisfies it -- "we tried N times and it failed N times" is a count
  of observations. A time budget does not: it converts ignorance into a
  verdict, which is exactly the failure this feature exists to prevent.

  So the two halves of "stuck" get opposite answers, because they are not the
  same situation:

  - **We do not know where this goes.** Routing has not exhausted its
    knowledge yet. Nothing accumulates here -- another day of not knowing is
    not more evidence than the first day of not knowing -- so this parks, with
    no cap of any kind. A user who was merely offline must never lose a
    message NMP never proved undeliverable.
  - **We know where it goes and it will not go.** The destination IS known and
    the relay keeps refusing. Each refusal is real evidence, and enough of
    them justify stopping at that relay.

  These were once the same rule, and treating them alike was wrong in both
  directions: it made an unresolved route abandonable by a clock, and it made
  a relay that is simply gone hold an obligation open forever.

  A third case sits between them and is neither: knowledge IS exhausted and it
  named zero relays. There is nowhere to publish, and saying so is a fact
  rather than a guess. That terminates immediately.

  Background:
    Given only the indexer relay "wss://indexer.example" is configured
    And my relay list names "wss://relay.mine.example" as my write relay
    And I am logged in as my own account

  Rule: An unresolved route parks, and no clock ever ends it

    # nmp:id=WRITES-PARKED-001
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Let a time budget abandon a write parked on an unresolved route; a user who was merely offline loses a message NMP never proved undeliverable.
    Scenario: Publishing before the first relay list has been fetched parks
      # The case an app hits on its very first run: publish immediately,
      # before any relay list has been fetched. Routing cannot answer yet --
      # which is a reason to wait, not a reason to destroy the obligation.
      Given no relay list has been fetched yet
      When I publish a note saying "first post"
      Then the receipt reports an open destination set naming no relays
      And the write is never reported as settled
      And the intent is still durably held

    # nmp:id=WRITES-PARKED-002
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Expire a write parked on an unresolved route after any elapsed time; the ninety-day park must still be held with its destination set still open.
    Scenario: No amount of time abandons a write parked on an unresolved route
      # #1136's surviving claim, and the one this rule exists to protect.
      # NMP can no more prove a recipient will never publish a relay list than
      # it can prove they will. Ninety days of not knowing is still not
      # knowing.
      Given nothing is known yet about Bob's DM relay list
      And I published a direct message to Bob and its destination set is still open
      When 90 days pass
      Then the receipt still reports an open destination set naming no relays
      And the intent is still durably held
      And nothing abandoned the write on NMP's own initiative

    # nmp:id=WRITES-PARKED-003
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Require the app to notice, retry or re-publish for an arriving relay list to unpark the write; the same write must go out, not a second copy.
    Scenario: A parked route resumes on its own when the knowledge arrives
      # Park means waiting, and waiting means it can end. Nothing about
      # unparking requires the app to notice, retry or re-publish anything.
      Given nothing is known yet about Bob's DM relay list
      And I published a direct message to Bob and its destination set is still open
      When Bob's DM relay list arrives naming "wss://inbox.bob.example"
      Then the write is routed to "wss://inbox.bob.example"
      And the receipt reports a destination set naming "wss://inbox.bob.example"
      And the same write is delivered -- not a second copy of it

    # nmp:id=WRITES-PARKED-004
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Drop the park across a restart; an app that reattaches to a persisted receipt id must not be told nothing, which is indistinguishable from data loss.
    Scenario: A park survives a restart and resumes after it
      Given nothing is known yet about Bob's DM relay list
      And I published a direct message to Bob and its destination set is still open
      When the process stops immediately
      And I reconstruct the engine from the same durable store
      And I reattach to the receipt by its stable id
      Then the receipt reports an open destination set naming no relays
      When Bob's DM relay list arrives naming "wss://inbox.bob.example"
      Then the write is routed to "wss://inbox.bob.example"

  Rule: A missing signer parks with no cap either, and the app is the only other exit

    # nmp:id=WRITES-PARKED-005
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Abandon a write awaiting a signer after any elapsed time; a device whose signer is simply not plugged in yet loses the write.
    Scenario: A write awaiting a signer is never abandoned by time
      # #1136's second surviving claim, generalised: a write NEVER ATTEMPTED
      # is not abandoned for the same reason as one attempted and failed.
      # There is no accumulating evidence here at all.
      Given no signer is registered for my account
      When I publish a note saying "hello"
      And 90 days pass
      Then the receipt reports the write awaiting a signer for my account
      And the write is never reported as settled
      And the intent is still durably held

    # nmp:id=WRITES-PARKED-006
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Resume a signer-parked write when any other signer attaches; the write must wait for the exact frozen key, never a currently-active substitute.
    Scenario: A signer-parked write resumes only for its own frozen identity
      Given no signer is registered for my account
      And I published a note and it parked awaiting a signer
      When a signer for a different account is registered
      Then the receipt still reports the write awaiting a signer for my account
      When a signer for my account is registered
      Then the write is signed

    # nmp:id=WRITES-PARKED-007
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Leave a signer-parked write in the queue after the app removes its entry; removal is the termination path, so an entry nothing will ever move must actually go.
    Scenario: Removing the queue entry is the other way a signer-parked write ends
      # The app's own decision, and the only alternative to waiting. This is
      # a termination path, not housekeeping.
      Given no signer is registered for my account
      And I published a note and it parked awaiting a signer
      When I enumerate my publish queue
      Then the entry reports the write awaiting a signer for my account
      When I remove that entry from my publish queue
      Then my publish queue no longer holds that entry

  Rule: Exhausted knowledge naming zero relays is terminal, not a park

    # nmp:id=WRITES-PARKED-008
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Park a write whose routing completed with zero relays; "we finished looking and there is nowhere to send this" would be reported as "we are still looking", which no app can act on.
    Scenario: A write whose routing finished with nowhere to go is terminal
      # This is the scenario that changed. "Settled that Bob has no DM relay
      # list" is knowledge EXHAUSTED, not knowledge missing -- and a write
      # with a closed, empty destination set has nowhere to publish. Saying
      # so is a fact; parking it forever would be a guess dressed as patience.
      Given the indexers have settled that Bob has no DM relay list
      When I publish a direct message to Bob
      Then the receipt reports a closed destination set naming no relays
      And the receipt reports the write as having no destination
      And the write is never reported as settled

    # nmp:id=WRITES-PARKED-009
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Report the same destination fact for a route still resolving and one that resolved to nothing; the two situations have opposite correct app behaviour and collapsing them is #1236.
    Scenario: Still resolving and resolved-to-nothing are told apart
      # The whole of #1236, dissolved. One flag, not one string: whether
      # resolution can still change its mind.
      Given nothing is known yet about Bob's DM relay list
      When I publish a direct message to Bob
      Then the receipt reports an open destination set naming no relays
      When the indexers settle that Bob has no DM relay list
      Then the receipt reports a closed destination set naming no relays
      And the receipt reports the write as having no destination

  Rule: A relay that keeps refusing is given up on, and only that relay

    # nmp:id=WRITES-PARKED-010
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Retry a known-unreachable relay without bound; the obligation is held open forever and the app is never told to stop, which is #1031's defect.
    Scenario: A relay that never accepts is eventually given up on
      # This scenario replaces "an unreachable relay is retried forever
      # rather than forgotten", which the ceiling ruling made false. The
      # destination IS known here -- that is what makes each refusal
      # evidence rather than ignorance.
      Given I am told to publish a note to exactly "wss://non-existent.example"
      When I publish that note
      And every attempt against "wss://non-existent.example" fails
      Then the receipt eventually reports "wss://non-existent.example" as given up on
      And the write is reported as settled

    # nmp:id=WRITES-PARKED-011
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Spend attempts while a lane is disconnected or waiting for authentication; time offline would become evidence, and a merely-offline user's write would be given up on.
    Scenario: Time offline is not evidence and spends no attempt
      # The ceiling counts observations. A lane that never got to try has
      # observed nothing, so no amount of being offline can exhaust it.
      Given I am told to publish a note to exactly "wss://offline.example"
      And "wss://offline.example" is unreachable
      When I publish that note
      And 90 days pass
      Then the receipt reports "wss://offline.example" as waiting for a connection
      And "wss://offline.example" is never reported as given up on

    # nmp:id=WRITES-PARKED-012
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Fail the whole write when one relay is given up on; three relays that published would be reported to the user as a failure.
    Scenario: Giving up on one relay is a footnote, not a failed write
      Given my relay list names these as my write relays:
        | wss://one.example   |
        | wss://two.example   |
        | wss://three.example |
      And every attempt against "wss://three.example" fails
      When I publish a note saying "hello"
      Then the receipt reports "wss://one.example" published
      And the receipt reports "wss://two.example" published
      And the receipt eventually reports "wss://three.example" as given up on
      And the write is reported as settled

  Rule: Cancellation and removal are the app's doors, and they stay distinct

    # nmp:id=WRITES-PARKED-013
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Let a timeout produce the same terminal fact as an explicit cancellation; nobody decided, and the app cannot tell its own decision from NMP's guess.
    Scenario: Cancellation is a decision somebody made
      Given nothing is known yet about Bob's DM relay list
      And I published a direct message to Bob and its destination set is still open
      When I cancel that write
      Then the receipt reports the write as not sent because it was cancelled
      And the intent is no longer held for delivery

    # nmp:id=WRITES-PARKED-014
    # nmp:status=specified
    # nmp:gap=evidence
    # nmp:issue=#1253
    # Defect shape this scenario will falsify once its evidence runs (#1253):
    #   Accept removal of a write that still owns live delivery lanes; removal would silently abandon in-flight work that cancellation exists to compensate.
    Scenario: Removal refuses a write that still owns live work
      Given I publish a note saying "hello"
      And the write still owns live delivery lanes
      When I try to remove that entry from my publish queue
      Then removal is refused because the write is still active
      And the intent is still durably held
