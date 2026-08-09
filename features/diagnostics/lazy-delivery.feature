Feature: Diagnostics changes are delivered without rebuilding every close
  Diagnostics describe current reducer state, but ordinary lifecycle changes
  do not need to materialize the same large snapshot repeatedly. NMP marks the
  state dirty, coalesces nearby changes behind one short delivery boundary,
  and still gives a newly attached observer the truthful current snapshot
  immediately.

  # nmp:id=DIAG-LAZY-001
  # nmp:status=built
  # nmp:evidence=rust:nmp::distinct_physical_closes_defer_diagnostic_coverage_projection
  # nmp:evidence=rust:nmp::one_due_delivery_fans_the_latest_full_snapshot_to_every_observer
  # nmp:falsifier=Build a full diagnostic snapshot for every physical close; an incompatible close burst performs triangular coverage and filter work instead of one bounded latest-state delivery.
  Scenario: A close burst produces bounded latest diagnostics
    Given many independent physical requests are active
    And at least one diagnostics observer is attached
    When those requests close in one burst
    Then each close marks diagnostics dirty without rebuilding the full snapshot
    And the first change anchors one bounded delivery window
    And one latest truthful snapshot is delivered to every observer at the boundary
    And no observer receives an intermediate stale snapshot

  # nmp:id=DIAG-LAZY-002
  # nmp:status=built
  # nmp:evidence=rust:nmp::new_observer_current_snapshot_satisfies_pending_delivery_without_duplicate
  # nmp:evidence=rust:nmp::engine_deadline_delivers_the_lazy_withdrawal_snapshot
  # nmp:falsifier=Register a diagnostics observer while a dirty delivery is pending but leave the old deadline armed; the observer receives the current state immediately and then receives an unchanged duplicate.
  Scenario: A new observer gets current truth without a duplicate deadline frame
    Given diagnostics changed and a bounded delivery is pending
    When a new diagnostics observer attaches before the deadline
    Then it receives the current full snapshot immediately
    And that immediate snapshot satisfies the pending delivery
    And neither existing nor new observers receive an unchanged duplicate at the old deadline

  # nmp:id=DIAG-LAZY-003
  # nmp:status=built
  # nmp:evidence=rust:nmp::nip77_barrier_lifecycle_is_lazy_without_a_diagnostics_observer
  # nmp:evidence=rust:nmp::eose_refreshes_live_evidence_without_event_index_query
  # nmp:evidence=rust:nmp::no_observer_arms_no_work_and_the_first_observed_change_anchors_the_window
  # nmp:falsifier=Materialize diagnostics inside ordinary EOSE, the NIP-77 live-first barrier, or fallback transitions without an observer; lifecycle work builds snapshots even though nobody can receive them, or later observation delivers more than one coalesced latest state.
  Scenario: Unobserved request terminals update state without building diagnostics
    Given no diagnostics observer is attached
    When ordinary EOSE, the NIP-77 live-first barrier, or a reconciliation fallback changes diagnostic state
    Then the reducer updates its truthful state immediately
    And it marks diagnostics dirty without materializing a snapshot
    And repeated changes arm no delivery work while there are no observers
    And a later observer receives one current snapshot through the bounded delivery contract
