Feature: Boot recovery costs what changed, not what accumulated
  Reopening an engine rebuilds volatile ownership from the durable write
  queue, and the engine thread finishes that rebuild before it reads its
  first command. So whatever recovery costs, the app's first call pays --
  and a rebuild whose cost follows the SIZE of the queue rather than the
  number of facts it has to record gets slower forever.

  Rule: Recovery records only facts that are not already durable

    # nmp:id=LIMITS-BOOTRECOVERY-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::boot_recovery_rewrites_no_lane_when_no_durable_fact_changed
    # nmp:evidence=rust:nmp-store::a_lane_bootstrap_that_stages_no_row_commits_nothing
    # nmp:falsifier=Commit the lane bootstrap that staged no row, or re-park an eligible lane whose relay is simply not connected yet; the durable lane set stops being identical across the reopen and the unstaged-bootstrap count stops being the whole population.
    Scenario: Reopening over a large queue rewrites nothing
      Given a durable write queue whose every lane is already eligible and unreached
      When the engine reopens and rebuilds ownership from that queue
      Then every lane keeps the exact revision and state it had before the reopen
      And the relays those lanes need are demanded as usual

  Rule: An obligation nothing can want is not carried to the next boot

    # nmp:id=LIMITS-BOOTRECOVERY-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::presence_renewals_leave_exactly_one_open_obligation
    # nmp:falsifier=Let a superseded renewal that never reached a relay survive its replacement; the open obligation count grows with the renewal count instead of staying at one.
    Scenario: Repeated presence renewals leave one obligation
      Given an app renews its kind 30315 status many times at one address
      And no relay is ever reached
      When the engine reopens and reads the durable write queue
      Then exactly one obligation is open at that address
      And it is the newest renewal
