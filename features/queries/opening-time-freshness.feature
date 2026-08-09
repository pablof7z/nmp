Feature: Opening-time freshness is separate from deadline maintenance
  Freshness is a one-time policy decision for the exact query an app opens.
  NMP owns the scoped relay-coverage watermark, the comparison with current
  wall time, the resulting acquisition decision, and durable restart recovery.
  Expiry, publication retry, and reconciliation liveness are scheduled engine
  work; opening an unrelated query is not their timer.

  Rule: Max-age freshness is scoped relay coverage, never cached row age

    # nmp:id=QUERIES-FRESHNESS-CLOCK-004
    # nmp:status=built
    # nmp:evidence=rust:nmp::fresh_cached_profile_uses_coverage_and_zero_wire
    # nmp:falsifier=Use the cached kind:0 created_at as freshness truth; the deliberately old profile creates a remote request despite recent scoped relay coverage.
    Scenario: An old cached profile is fresh when its scoped relay coverage is recent
      Given a cached kind:0 document is older than the permitted maximum age
      And every relay assigned to that exact demand has a recent coverage watermark
      When the app opens the max-age query
      Then the old cached document is delivered in the opening snapshot
      And the query creates no remote request

    # nmp:id=QUERIES-FRESHNESS-CLOCK-005
    # nmp:status=built
    # nmp:evidence=rust:nmp::stale_max_age_is_live_but_recent_empty_coverage_is_fresh
    # nmp:falsifier=Treat a cached kind:0 row as proof of freshness; the immediate cached row remains but the ordinary live remote request disappears.
    Scenario: Cached data is immediate while stale or missing coverage becomes live
      Given a matching kind:0 document already exists in the cache
      And its exact demand has stale or missing scoped relay coverage
      When the app opens the max-age query
      Then the cached document is delivered in the opening snapshot
      And the query creates its ordinary live remote request

    # nmp:id=QUERIES-FRESHNESS-CLOCK-006
    # nmp:status=built
    # nmp:evidence=rust:nmp::stale_max_age_is_live_but_recent_empty_coverage_is_fresh
    # nmp:falsifier=Require a matching cached row before coverage can satisfy MaxAge; the recently covered empty answer incorrectly creates a remote request.
    Scenario: Recent empty coverage can satisfy max-age freshness
      Given every relay assigned to the exact demand recently covered its question
      And no matching event exists in the cache
      When the app opens the max-age query
      Then the opening snapshot is empty
      And the query creates no remote request

    # nmp:id=QUERIES-FRESHNESS-CLOCK-007
    # nmp:status=built
    # nmp:evidence=rust:nmp::nested_max_age_scoped_coverage_survives_redb_restart
    # nmp:falsifier=Keep scoped coverage only in reducer memory; reopening the durable store loses the watermark and creates a nested remote request.
    Scenario: Durable scoped coverage still informs max-age after restart
      Given recent scoped coverage is committed to the durable store
      When the engine is reconstructed from that store
      And the app reopens the max-age query
      Then NMP uses the persisted scoped watermark for the opening decision
      And the covered demand creates no remote request

  Rule: Only max-age freshness compares coverage with current wall time

    # nmp:id=QUERIES-FRESHNESS-CLOCK-001
    # nmp:status=built
    # nmp:evidence=rust:nmp::subscribe_uses_current_wall_clock_for_the_one_time_max_age_decision
    # nmp:falsifier=Use the reducer's last maintenance time for MaxAge; stale persisted coverage is treated as fresh and the required relay request disappears.
    Scenario: A max-age query uses current time without running maintenance
      Given a query permits cached coverage no older than one second
      And its persisted relay coverage is sixty seconds old
      When the app opens that query
      Then the query contributes its ordinary live relay request
      And opening it does not run expiry, retry, or liveness maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-002
    # nmp:status=built
    # nmp:evidence=rust:nmp::many_live_and_cache_only_opens_run_zero_maintenance_sweeps
    # nmp:falsifier=Call Tick before every observation open; 207 Live and CacheOnly opens call the store expiration door 207 times with no deadline due.
    Scenario: Live and cache-only opens are not maintenance events
      Given no engine deadline is due
      When the app opens many live and cache-only queries
      Then no store expiration sweep runs
      And no publication retry or reconciliation liveness sweep runs

  Rule: A due deadline wins a race with an app command

    # nmp:id=QUERIES-FRESHNESS-CLOCK-003
    # nmp:status=built
    # nmp:evidence=rust:nmp::due_deadline_runs_before_a_simultaneously_ready_command
    # nmp:falsifier=Dispatch the command returned by recv_timeout before checking its armed core deadline; the engine blocks inside the command while the exactly-due event remains visible.
    Scenario: An exactly-due expiration runs before a simultaneous command
      Given an expiring cached event and an observation that currently sees it
      And the next engine deadline is that event's expiration
      When an app command becomes ready at exactly the same instant
      Then the event is retracted before the command is dispatched
      And the deadline is consumed exactly once

  Rule: Delayed wire admission owns the liveness time it creates

    # nmp:id=QUERIES-FRESHNESS-CLOCK-008
    # nmp:status=built
    # nmp:evidence=rust:nmp::nip77_liveness_is_anchored_to_admission_time_without_maintenance
    # nmp:falsifier=Leave reducer clock truth at its old value when the delayed admission flush creates a NIP-77 handoff; its liveness deadline is immediately stale instead of one full window after admission.
    Scenario: A delayed NIP-77 handoff gets a full liveness window
      Given the reducer's last maintenance time is old
      And a broad live query is waiting for admission on a behaviorally proven NIP-77 relay
      When the pending cohort is admitted at the current wall time
      Then the handoff deadline is one full liveness window after admission
      And stamping the admission time runs no deadline maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-009
    # nmp:status=built
    # nmp:evidence=rust:nmp::nip77_reconnect_liveness_is_anchored_to_connect_time_without_maintenance
    # nmp:falsifier=Replay a proven NIP-77 request after a long idle without advancing cheap clock truth; the new generation inherits an already-stale liveness deadline and immediately falls back.
    Scenario: A reconnected NIP-77 relay gets a fresh liveness window
      Given a planned broad request belongs to a behaviorally proven NIP-77 relay
      And the reducer's last maintenance time is old
      When a fresh relay generation connects at the current wall time
      Then its handoff deadline is one full liveness window after connection
      And stamping the connection time runs no deadline maintenance

  Rule: Command-time truth is separate from deadline maintenance

    # nmp:id=QUERIES-FRESHNESS-CLOCK-010
    # nmp:status=built
    # nmp:evidence=rust:nmp::waiting_connection_attempt_is_anchored_to_connect_time_without_maintenance
    # nmp:falsifier=Wake a durable lane after a long idle without advancing cheap clock truth; its persisted attempt starts at the old maintenance time instead of the current command time.
    Scenario: A parked durable write starts at current command time
      Given a durable write is waiting for its relay connection
      And the reducer's last maintenance time is old
      When its relay connects at the current wall time
      Then the persisted attempt starts at the connection time
      And advancing command-time truth runs no deadline maintenance
