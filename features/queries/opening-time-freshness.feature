Feature: Opening-time freshness is separate from deadline maintenance
  Freshness is a one-time policy decision for the exact query an app opens.
  NMP owns scoped relay coverage and the current-time comparison. Expiry,
  publication retry, and reconciliation liveness are scheduled engine work;
  opening an unrelated query is not their timer.

  Rule: Only max-age freshness compares coverage with current wall time

    # nmp:id=QUERIES-FRESHNESS-CLOCK-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::subscribe_uses_current_wall_clock_for_the_one_time_max_age_decision
    # nmp:falsifier=Use the reducer's last maintenance time; stale coverage is treated as fresh and the required request disappears.
    Scenario: A max-age query uses current time without running maintenance
      Given a query permits cached coverage no older than one second
      And its persisted relay coverage is sixty seconds old
      When the app opens that query
      Then the query contributes its ordinary live relay request
      And opening it does not run expiry, retry, or liveness maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::many_live_and_cache_only_opens_run_zero_maintenance_sweeps
    # nmp:falsifier=Call Tick before every open; 207 Live and CacheOnly opens call store expiration 207 times with no deadline due.
    Scenario: Live and cache-only opens are not maintenance events
      Given no engine deadline is due
      When the app opens many live and cache-only queries
      Then no store expiration sweep runs
      And no publication retry or reconciliation liveness sweep runs

    # nmp:id=QUERIES-FRESHNESS-CLOCK-011
    # nmp:status=built
    # nmp:evidence=rust:nmp::fresh_max_age_reads_each_coverage_row_once
    # nmp:falsifier=Read fresh coverage once for the decision and again for the opening frame; one satisfied observation performs two identical store reads.
    Scenario: A fresh max-age opening reuses its exact coverage proof
      Given persisted relay coverage satisfies a max-age query
      When the app opens that query
      Then each assigned coverage row is read once
      And the opening evidence retains the watermark that justified no wire

    # nmp:id=QUERIES-FRESHNESS-CLOCK-012
    # nmp:status=built
    # nmp:evidence=rust:nmp::max_age_opening_retains_only_its_scoped_candidate_plan
    # nmp:evidence=rust:nmp-router::one_preview_never_visits_ten_thousand_unrelated_incumbent_demand_edges
    # nmp:falsifier=Recompile and retain the whole active plan for one fresh opening; 207 unrelated sources are retained and the 10,000-edge preview visits incumbents.
    Scenario: A max-age opening evaluates only its own scoped relay work
      Given many unrelated live observations are already open
      And a new max-age query has fresh coverage at its assigned relay
      When the app opens the new query
      Then NMP evaluates only the new query against current relay capacity
      And the opening decision retains only the new query's assigned source
      And unrelated requests are neither reconsidered nor retained

  Rule: A due deadline wins a race with an app command

    # nmp:id=QUERIES-FRESHNESS-CLOCK-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::due_deadline_runs_before_a_simultaneously_ready_command
    # nmp:falsifier=Dispatch the ready command before its armed deadline; the engine blocks while an exactly-due event remains visible.
    Scenario: An exactly-due expiration runs before a simultaneous command
      Given an expiring cached event and an observation that currently sees it
      And the next engine deadline is that event's expiration
      When an app command becomes ready at exactly the same instant
      Then the event is retracted before the command is dispatched
      And the deadline is consumed exactly once

  Rule: Delayed work owns the current time it stamps

    # nmp:id=QUERIES-FRESHNESS-CLOCK-008
    # nmp:status=built
    # nmp:evidence=rust:nmp::nip77_liveness_is_anchored_to_admission_time_without_maintenance
    # nmp:falsifier=Let delayed admission reuse old clock truth; its new NIP-77 handoff deadline is already stale.
    Scenario: A delayed NIP-77 handoff gets a full liveness window
      Given the reducer's last maintenance time is old
      And a broad live query is waiting for admission on a proven NIP-77 relay
      When the pending cohort is admitted at the current wall time
      Then the handoff deadline is one full liveness window after admission
      And stamping the admission time runs no deadline maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-009
    # nmp:status=built
    # nmp:evidence=rust:nmp::nip77_reconnect_liveness_is_anchored_to_connect_time_without_maintenance
    # nmp:falsifier=Reconnect after a long idle without cheap clock advance; the new generation inherits an already-stale deadline.
    Scenario: A reconnected NIP-77 relay gets a fresh liveness window
      Given a planned broad request belongs to a proven NIP-77 relay
      And the reducer's last maintenance time is old
      When a fresh relay generation connects at the current wall time
      Then its handoff deadline is one full liveness window after connection
      And stamping the connection time runs no deadline maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-010
    # nmp:status=built
    # nmp:evidence=rust:nmp::waiting_connection_attempt_is_anchored_to_connect_time_without_maintenance
    # nmp:falsifier=Wake a durable lane after a long idle without cheap clock advance; its attempt starts at stale maintenance time.
    Scenario: A parked durable write starts at current command time
      Given a durable write is waiting for its relay connection
      And the reducer's last maintenance time is old
      When its relay connects at the current wall time
      Then the persisted attempt starts at the connection time
      And advancing command-time truth runs no deadline maintenance
