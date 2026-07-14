# Diagnostics

Use two different proof surfaces for two different questions:

- `RowBatch.evidence` answers what sources and shortfalls apply to one query subtree.
- the diagnostics stream describes the engine-global current relay plan and observed wire facts.

Current cross-platform relay diagnostics expose relay URL, wire subscription count, authors served, lane counts, exact wire-filter JSON, events received by kind, and per-filter coverage intervals. The snapshot also exposes uncovered author count, dropped merge rules, and transport degradation on all supported tiers.

Rust additionally exposes discovered-private-relay rejection count, over-cap rejection count, `store_degraded`, and transport degradation. Swift/Kotlin do not currently project `store_degraded` or the two rejection counters. Do not design a native recovery screen around fields it cannot observe.

Do not claim that diagnostics currently provide:

- demand graph nodes or refcounts;
- write-intent/receipt queues, retry schedule, or write attempts;
- scheduler/queue-pressure telemetry;
- public pending-row signature state;
- a global connection generation or populated AUTH lifecycle;
- database row counts or GC telemetry.

`SourceStatus.awaitingAuth`/`authDenied` and `AuthPhase` exist as reserved public vocabulary but are not populated by the current engine. Label them reserved if they appear in exhaustive UI switches.

For debugging, correlate the query's exact filter and evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Preserve absence as absence: no coverage row is unproven, not zero or complete.
