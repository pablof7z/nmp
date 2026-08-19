Feature: Opening-time freshness is separate from deadline maintenance
  Freshness is a one-time policy decision for the exact query an app opens.
  NMP owns scoped relay coverage and the current-time comparison. Expiry,
  publication retry, and reconciliation liveness are scheduled engine work;
  opening an unrelated query is not their timer.

  Rule: Only max-age freshness compares coverage with current wall time

    Scenario: A max-age query uses current time without running maintenance
      Given a query permits cached coverage no older than one second
      And its persisted relay coverage is sixty seconds old
      When the app opens that query
      Then the query contributes its ordinary live relay request
      And opening it does not run expiry, retry, or liveness maintenance

    Scenario: Live and cache-only opens are not maintenance events
      Given no engine deadline is due
      When the app opens many live and cache-only queries
      Then no store expiration sweep runs
      And no publication retry or reconciliation liveness sweep runs

    Scenario: A fresh max-age opening reuses its exact coverage proof
      Given persisted relay coverage satisfies a max-age query
      When the app opens that query
      Then each assigned coverage row is read once
      And the opening evidence retains the watermark that justified no wire

    Scenario: A max-age opening evaluates only its own scoped relay work
      Given many unrelated live observations are already open
      And a new max-age query has fresh coverage at its assigned relay
      When the app opens the new query
      Then NMP evaluates only the new query against current relay capacity
      And the opening decision retains only the new query's assigned source
      And unrelated requests are neither reconsidered nor retained

  Rule: A due deadline wins a race with an app command

    Scenario: An exactly-due expiration runs before a simultaneous command
      Given an expiring cached event and an observation that currently sees it
      And the next engine deadline is that event's expiration
      When an app command becomes ready at exactly the same instant
      Then the event is retracted before the command is dispatched
      And the deadline is consumed exactly once

  Rule: Delayed work owns the current time it stamps

    Scenario: A delayed NIP-77 handoff gets a full liveness window
      Given the reducer's last maintenance time is old
      And a broad live query is waiting for admission on a proven NIP-77 relay
      When the pending cohort is admitted at the current wall time
      Then the handoff deadline is one full liveness window after admission
      And stamping the admission time runs no deadline maintenance

    Scenario: A reconnected NIP-77 relay gets a fresh liveness window
      Given a planned broad request belongs to a proven NIP-77 relay
      And the reducer's last maintenance time is old
      When a fresh relay generation connects at the current wall time
      Then its handoff deadline is one full liveness window after connection
      And stamping the connection time runs no deadline maintenance

    Scenario: A parked durable write starts at current command time
      Given a durable write is waiting for its relay connection
      And the reducer's last maintenance time is old
      When its relay connects at the current wall time
      Then the persisted attempt starts at the connection time
      And advancing command-time truth runs no deadline maintenance
