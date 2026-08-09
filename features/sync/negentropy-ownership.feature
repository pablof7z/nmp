Feature: Reconciliation work keeps exact plan ownership
  NIP-77 may create temporary child requests for a logical plan. Those child
  requests remain owned and retired through that exact plan without scanning
  or borrowing state from unrelated plans.

  # nmp:id=SYNC-NEGENTROPY-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::probed_nip77_plan_closes_touch_only_their_exact_children
  # nmp:falsifier=Find a plan's pending live handoff, reconciliation session, or temporary backfill by scanning every other plan's children; closing many probed plans performs triangular teardown work or retains child ownership.
  Scenario: Closing a probed plan retires only its own reconciliation children
    Given many independent plans are active on a relay proven to support reconciliation
    And each plan owns its own pending handoff or repair children
    When every plan withdraws independently
    Then each close visits only the children owned by that exact plan
    And no sibling plan or child is scanned or retired
    And the final close leaves no live, pending, or reverse reconciliation ownership

  # nmp:id=SYNC-NEGENTROPY-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::reconnect_repeats_live_first_and_only_the_fresh_generation_eose_opens_neg
  # nmp:evidence=rust:nmp::pending_execution_census_counts_every_revision_queued_under_one_wire_key
  # nmp:falsifier=Remove stale reconnect children only from their primary map or count only a pending queue key; reverse ownership survives the generation change, or multiple queued request revisions appear as one owner and a zero census can pass with retained containers.
  Scenario: Reconnect replacement and pending queues have exact ownership
    Given a probed plan has a live-first candidate owned by one transport generation
    When that relay reconnects on a new generation
    Then stale candidate ownership is removed through both the primary and plan-child indexes
    And only the fresh generation's candidate can open reconciliation
    And a request key with multiple pending revisions reports one key and every queued owner
    And final cancellation leaves zero primary entries, reverse entries, queue keys, and queue owners
