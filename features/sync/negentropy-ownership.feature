Feature: Reconciliation work keeps exact plan ownership
  NIP-77 may create temporary child requests for a logical plan. Those child
  requests remain owned and retired through that exact plan without scanning
  or borrowing state from unrelated plans.

  Scenario: Closing a probed plan retires only its own reconciliation children
    Given many independent plans are active on a relay proven to support reconciliation
    And each plan owns its own pending handoff or repair children
    When every plan withdraws independently
    Then each close visits only the children owned by that exact plan
    And no sibling plan or child is scanned or retired
    And the final close leaves no live, pending, or reverse reconciliation ownership

  Scenario: Reconnect replacement and pending queues have exact ownership
    Given a probed plan has a live-first candidate owned by one transport generation
    When that relay reconnects on a new generation
    Then stale candidate ownership is removed through both the primary and plan-child indexes
    And only the fresh generation's candidate can open reconciliation
    And a request key with multiple pending revisions reports one key and every queued owner
    And final cancellation leaves zero primary entries, reverse entries, queue keys, and queue owners

  Scenario: A predecessor can finish reconciliation handoff while its replacement waits
    Given a proven reconciliation relay has accepted a live candidate for the current plan
    And a byte-changed successor request is waiting for local transport acceptance
    When the predecessor candidate reaches EOSE before that successor is accepted
    Then the predecessor still owns the exact plan metadata needed to open reconciliation
    And NMP does not panic or discard the predecessor's valid completion
    And accepting the successor later retires the predecessor exactly once
    And final withdrawal leaves no plan, request, or reconciliation ownership
