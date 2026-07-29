Feature: Everything that cannot progress, in one place, without a receipt
  A receipt answers "what happened to THIS write", which is only useful to
  someone still holding it. The question an app actually needs answered when
  it opens is the other one: is anything quietly stuck? Nobody is holding the
  receipt for the DM composed on a train three weeks ago, and that is exactly
  the write worth surfacing.

  So the second half of "we were trying to publish to relay X and it didn't
  work" is a global view, next to the diagnostics an app already reads. Every
  obligation that cannot progress appears in it, whatever stage it is stuck
  at, and there are three stages it can be stuck at: **unroutable** -- no
  destination could be computed; **unsignable** -- no signer is available for
  the author it was frozen to; **undeliverable** -- destinations exist and
  none of them are working. An app that has to look in three places to answer
  one question will look in none of them.

  Each entry carries an age, and the age is what makes this a diagnostic
  rather than a list. A DM parked for forty seconds is discovery in flight
  and means nothing. The same DM parked for forty days is the case the whole
  design started from -- a recipient who never published a relay list -- and
  it means the send will never happen. NMP reports the number and refuses to
  interpret it, because interpreting it is deciding to give up, and giving up
  is the app's decision or the person's, never a timer's.

  The relay that does not exist lands here too, and it is the cleanest
  illustration of why acceptance cannot be the place this is caught:

  > we can't know if the user says "when you go online publish this to
  > wss://non-existent.com"

  Routing that write is instantaneous and completely successful -- the
  destination is precisely the one named. It is delivery that will never
  happen, forever, and nothing at acceptance time could have said so.

  Background:
    Given I am logged in as my own account
    And my relay list names "wss://relay.mine.example" as my write relay

  # ---- visible without holding the receipt ------------------------------

  @designed @ledger-9
  Scenario: Stuck work is visible to an app holding no receipt
    Given the indexers have settled that Bob has no DM relay list
    And a direct message to Bob was published and parked awaiting a route
    And I hold no receipt for that write
    When I read diagnostics
    Then stalled writes names that write
    And it reports the write as unroutable
    And it reports the reason as Bob's missing DM relay list
    And it reports how long the write has been stalled

  @designed @ledger-9
  Scenario: All three ways of being stuck appear in the same list
    # The point of a single list. These three writes are stuck at three
    # different stages for three unrelated reasons, and an app asking "is
    # anything wrong" asks once.
    Given the indexers have settled that Bob has no DM relay list
    And a direct message to Bob was published and parked awaiting a route
    And a NIP-46 signer is registered for the current pubkey but is offline
    And a note saying "waiting to be signed" was published and is unsigned
    And relay "wss://relay.mine.example" cannot be connected to
    And a note saying "nowhere to land" was published and signed
    When I read diagnostics
    Then stalled writes reports 3 writes
    And one of them is reported as unroutable
    And one of them is reported as unsignable
    And one of them is reported as undeliverable
    And each of them reports its own reason

  @ledger-9
  Scenario: The relay that does not exist
    # Pablo's own example. Routing succeeded perfectly and instantly; the
    # world is what refuses. Nothing about this write was ever malformed and
    # nothing at acceptance could have detected it.
    Given I am told to publish a note to exactly "wss://non-existent.example"
    When I publish that note
    Then the write is accepted
    And the write was routed to "wss://non-existent.example"
    When I read diagnostics
    Then stalled writes names that write
    And it reports the write as undeliverable
    And it reports the reason as "wss://non-existent.example" being unreachable
    And it reports how long the write has been stalled

  # ---- the age, and what NMP declines to do with it ---------------------

  @designed @ledger-9
  Scenario Outline: The age is reported; what it means is not NMP's call
    # Forty seconds and forty days are the same fact with different numbers,
    # and NMP treats them identically on purpose. Only the app knows whether
    # its user cares.
    Given the indexers have settled that Bob has no DM relay list
    And a direct message to Bob was published and parked awaiting a route
    When <elapsed> pass
    And I read diagnostics
    Then stalled writes names that write
    And it reports the write as stalled for about <elapsed>
    And NMP has drawn no conclusion from how long it has been stalled

    Examples:
      | elapsed    |
      | 40 seconds |
      | 40 days    |

  @designed @ledger-16
  Scenario: Nothing ages out of the list
    Given the indexers have settled that Bob has no DM relay list
    And a direct message to Bob was published and parked awaiting a route
    When 365 days pass
    And I read diagnostics
    Then stalled writes names that write
    And the intent is still durably held
    And nothing abandoned the write on NMP's own initiative

  @designed @ledger-9
  Scenario: Work that starts moving again leaves the list
    # The list has to be able to empty, or an app learns to ignore it.
    Given nothing is known yet about Bob's DM relay list
    And a direct message to Bob was published and parked awaiting a route
    When I read diagnostics
    Then stalled writes names that write
    When Bob's DM relay list arrives naming "wss://inbox.bob.example"
    And the write is delivered to "wss://inbox.bob.example"
    And I read diagnostics
    Then stalled writes does not name that write

  # ---- the list is evidence, and evidence does not act ------------------

  @designed @ledger-16
  Scenario: The list survives a restart, rebuilt from what was persisted
    # An app that restarts and sees an empty list would conclude everything
    # is fine. The list is a projection of durable obligations, not a tally
    # kept in memory by whoever happened to be running.
    Given the indexers have settled that Bob has no DM relay list
    And a direct message to Bob was published and parked awaiting a route
    When the process stops immediately
    And I reconstruct the engine from the same durable store
    And I read diagnostics
    Then stalled writes names that write
    And it reports the reason as Bob's missing DM relay list
    And it reports the write as having been stalled since before the restart

  Scenario: Reading the list changes nothing
    # Diagnostics is a mirror. If reading it retried, an app that polled would
    # publish differently from an app that did not, and the diagnostic would
    # be part of the system it claims to describe.
    Given relay "wss://relay.mine.example" cannot be connected to
    And a note saying "nowhere to land" was published and signed
    When I read diagnostics 100 times
    Then no delivery attempt was made by reading diagnostics
    And no write changed state
    And nothing durable was recorded
