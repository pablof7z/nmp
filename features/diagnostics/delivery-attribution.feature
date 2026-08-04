Feature: Which destination failed, and why, one destination at a time
  > This way of popping up a "we were trying to publish to relay X and it
  > didn't work" or "we were trying to route this event but it didn't work"
  > needs ato exist because there are many ways we'll find ourselves there

  This file is the first half of that: relay X, and what it said. A note sent
  to four relays is four independent attempts with four independent fates,
  and an app handed one aggregate verdict cannot tell a user which one broke
  or whether it is worth caring about. So every destination carries its own
  outcome, and the outcomes are kept distinct because apps do different
  things with them.

  A relay's own refusal comes back with the relay's own words -- "blocked:
  not admitted" is actionable and "failed" is not, and NMP has no business
  paraphrasing a message it did not write. A transient failure is
  retry-eligible and says when, so an app can show "retrying shortly" rather
  than an error. A destination given up on has finished trying and says so.
  And an unknown outcome -- an attempt that crossed a process loss while
  already in flight -- is the honest answer when the answer cannot be
  recovered: not a success, not a failure, and never retried on a write that
  may be attempted at most once.

  The second thing an app needs is the shape of the whole. Some destinations
  acked while others are still going is the ordinary case, not an edge case,
  and "sent" versus "partially sent" is a difference a user notices. It is
  representable here without the app inferring it from a pile of per-relay
  facts, and without NMP rounding it up into a single optimistic claim.

  Background:
    Given I am logged in as my own account
    And my relay list names these as my write relays:
      | wss://one.example   |
      | wss://two.example   |
      | wss://three.example |
      | wss://four.example  |

  # ---- one destination, one answer --------------------------------------

  @ledger-9
  Scenario: A relay's refusal is reported in the relay's own words
    Given relay "wss://one.example" rejects every event with "blocked: not admitted"
    When I publish a note saying "hello"
    Then the receipt reports "wss://one.example" rejected the note
    And the reason is the relay's own words "blocked: not admitted"

  # ---- authentication denial is not authentication waiting -------------

  @ledger-9 @ledger-16
  Scenario: A policy denial finishes the exact write lane and survives restart
    Given relay "wss://one.example" requires authentication for writes
    And my authentication policy denies "wss://one.example" with "account not permitted"
    When I publish a note saying "hello"
    Then the receipt reports "wss://one.example" as authentication denied by policy
    And the reason is the policy's own words "account not permitted"
    When the process stops immediately
    And I reconstruct the engine from the same durable store
    And I reattach to the receipt by its stable id
    Then the receipt reports "wss://one.example" as authentication denied by policy
    And the reason is the same reason it was denied with
    And no further event attempt is made against "wss://one.example"

  @designed @ledger-9
  Scenario: Authentication required is resumable rather than terminal
    Given relay "wss://one.example" requires authentication for writes
    And my authentication policy allows "wss://one.example"
    When I publish a note saying "hello"
    Then the receipt reports "wss://one.example" as awaiting authentication
    And "wss://one.example" is not reported as authentication denied
    And "wss://one.example" is not reported as retry-eligible
    When authentication succeeds for "wss://one.example"
    Then the same write is delivered -- not a second copy of it

  @designed @ledger-9
  Scenario: A subscription authentication closure cannot deny a write
    Given I have a pending write for "wss://one.example"
    And I have a separate subscription on "wss://one.example"
    When that subscription is closed as "auth-required"
    Then the write remains nonterminal
    And "wss://one.example" is not reported as authentication denied
    When exact-session authentication succeeds for the write
    Then the same write is delivered -- not a second copy of it

  @designed @ledger-9
  Scenario: Authentication denial is isolated by exact session identity
    Given Alice and Bob each have a pending authenticated write for "wss://one.example"
    When Alice's exact authentication session is denied
    Then only Alice's write is reported as authentication denied
    And Bob's write remains live
    And Bob's write can still be delivered

  @designed @ledger-9
  Scenario Outline: A non-denial authentication outcome cannot deny a write
    Given relay "wss://one.example" requires authentication for writes
    And my authentication policy returns "<outcome>" for "wss://one.example"
    When I publish a note saying "hello"
    Then the write remains nonterminal
    And "wss://one.example" is not reported as authentication denied

    Examples:
      | outcome     |
      | error       |
      | unavailable |

  @designed @ledger-16
  Scenario: A transient failure says why it will be retried, and when
    Given relay "wss://one.example" fails the first attempt transiently
    When I publish a note saying "hello"
    Then the receipt reports "wss://one.example" as retry-eligible
    And it reports which attempt that was
    And it reports when the next attempt becomes eligible
    And it reports the persisted non-authentication cause and relay detail
    And "wss://one.example" is not reported as failed

  @designed @ledger-16
  Scenario: A destination given up on is named as given up on
    # Distinct from retry-eligible on purpose: one is a promise that something
    # will happen next and the other is a statement that nothing will. Note
    # that this is one lane finishing its policy, not the write being
    # abandoned -- the write is still held, and still visible.
    Given relay "wss://one.example" fails every attempt transiently
    When I publish a note saying "hello"
    And that relay's attempts are exhausted
    Then the receipt reports "wss://one.example" as given up on
    And no further attempt is made against "wss://one.example"
    And the intent is still durably held

  @designed @ledger-9
  Scenario: An outcome nobody can recover is reported as unknown
    # The attempt was in flight when the process died. It may have landed. It
    # may not have. Reporting either would be a claim NMP cannot support, and
    # retrying would break an at-most-once write's one guarantee.
    Given a write that must be attempted at most once
    And its attempt against "wss://one.example" crossed a process loss
    When I reattach to the receipt by its stable id
    Then the receipt reports "wss://one.example" as outcome unknown
    And "wss://one.example" is not reported as acked
    And "wss://one.example" is not reported as failed
    And "wss://one.example" is never attempted again

  # ---- four destinations, four answers ----------------------------------

  @designed @ledger-9
  Scenario: One note, four relays, four different answers
    # The composite, and the reason an outcome is per-destination rather than
    # per-write. An app rendering this screen can name the relay that broke,
    # quote what it said, and leave the other three alone.
    Given relay "wss://one.example" accepts every event
    And relay "wss://two.example" rejects every event with "invalid: bad signature"
    And relay "wss://three.example" fails the first attempt transiently
    And relay "wss://four.example" cannot be connected to
    When I publish a note saying "hello"
    Then the receipt reports the note acked by "wss://one.example"
    And the receipt reports "wss://two.example" rejected the note
    And the reason is the relay's own words "invalid: bad signature"
    And the receipt reports "wss://three.example" as retry-eligible
    And the receipt reports "wss://four.example" as awaiting connectivity
    And each destination's outcome is reported independently of the others

  # ---- sent, partially sent, and the difference -------------------------

  @designed @ledger-9
  Scenario: Partially sent is representable without the app guessing
    Given relay "wss://one.example" accepts every event
    And relay "wss://two.example" accepts every event
    And relay "wss://three.example" fails the first attempt transiently
    And relay "wss://four.example" cannot be connected to
    When I publish a note saying "hello"
    Then the receipt reports 2 of 4 destinations acked
    And the receipt reports that some destinations have not finished
    And the receipt makes no claim that the note is fully sent

  @designed @ledger-9
  Scenario: Fully sent means every destination acked, and nothing weaker
    Given every one of my write relays accepts every event
    When I publish a note saying "hello"
    Then the receipt reports 4 of 4 destinations acked
    And the receipt reports that no destination is still outstanding

  @designed @ledger-9
  Scenario: An unknown outcome is never counted as sent
    # The tempting rounding error. Three acked and one unknowable is not four
    # acked, and an app told "sent" here would be told something false.
    Given relay "wss://one.example" accepts every event
    And relay "wss://two.example" accepts every event
    And relay "wss://three.example" accepts every event
    And a write that must be attempted at most once
    And its attempt against "wss://four.example" crossed a process loss
    When I reattach to the receipt by its stable id
    Then the receipt reports 3 of 4 destinations acked
    And the receipt reports 1 destination whose outcome is unknown
    And the receipt makes no claim that the note is fully sent

  # ---- the boundary between the two failures ----------------------------

  @designed @ledger-9
  Scenario: A write that never got a destination has no delivery failure
    # "We were trying to publish to relay X" and "we were trying to route this
    # event" are different sentences and an app shows different screens for
    # them. A write with no destinations yet has nothing to attribute a
    # delivery failure to, and must not invent one.
    Given the indexers have settled that Bob has no DM relay list
    When I publish a direct message to Bob
    Then the receipt reports the write awaiting a route
    And the receipt reports no destination as failed
    And the receipt reports no destination at all
