Feature: Replaceable writes keep only useful delivery work
  @ledger-9

  Scenario Outline: A newer offline replaceable write retires the older obligation
    Given my relay list names "offline-relay" as my write relay
    And I am logged in as my own account
    When relay "offline-relay" drops the connection
    And I publish kind <kind> with d tag "<d>" saying "older"
    Then the first receipt reports waiting for "offline-relay"
    When I publish kind <kind> with d tag "<d>" saying "newer"
    Then the first receipt reports superseded by the newer replaceable write
    And the second receipt reports waiting for "offline-relay"
    When relay "offline-relay" comes back
    Then the second receipt reports acked by "offline-relay"

    Examples:
      | kind  | d        |
      | 0     | ignored  |
      | 3     | ignored  |
      | 10001 | ignored  |
      | 30001 | presence |

  Rule: Obsolete local attempts are not durable history

    # A receipt is safety evidence for work that may have crossed a handoff.
    # It is not an archive of bytes NMP proved it never sent. Replaceable state
    # makes that distinction exact: after a newer value wins, an older unsent
    # value can never become useful again.

    Scenario: A newer replaceable write destroys its unsent predecessor
      Given an unpublished kind 0 write for my account
      When I publish a newer kind 0 write for the same account
      Then only the newer write remains in my publish queue
      And the older event body and delivery work no longer exist after restart
      And cancelling the newer write does not restore the older unpublished value
      And a started attempt explicitly proven not handed off is treated as unsent

    Scenario: An already-expired attempt never becomes durable write history
      Given a presence event whose expiration has already passed
      When I try to publish it
      Then publishing is refused before NMP takes custody
      And no event, receipt, signer request, relay lane, or attempt is retained

    # Some replacement evidence cannot disappear immediately: bytes may have
    # crossed a transport handoff, and forgetting that uncertainty would invite
    # an unsafe blind retry. It therefore follows the same terminal-receipt
    # retention rule as every other completed write, with no special class.

    Scenario: Replaced-write safety evidence follows the global terminal FIFO
      Given replaceable writes whose older values may have crossed a handoff
      Then their old event bodies and delivery machinery are permanently removed
      And only a terminal safety receipt represents the possible handoff
      And its terminal says the write was superseded after a possible handoff, not never sent
      When terminal receipt retention reaches its internal boundary
      Then replaced-write evidence competes in the same oldest-first order as every other terminal receipt
      But current obligations are never classified as disposable
      And possible-handoff ambiguity is retained only as that bounded safety evidence

    Scenario: Corrupt delivery evidence is never mistaken for safe deletion
      Given a persisted attempt says it was not handed off
      But the rest of that attempt record is incomplete or malformed
      When a newer replaceable write would retire the old attempt
      Then NMP refuses the replacement transaction as corrupt state
      And the old receipt and correlation remain untouched
      And no partial deletion is committed
