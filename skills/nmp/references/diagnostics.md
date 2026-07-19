# Diagnostics

Use two different proof surfaces for two different questions:

- `RowBatch.evidence` answers what sources and shortfalls apply to one query subtree.
- the diagnostics stream describes the engine-global current relay plan and observed wire facts.

Current cross-platform relay diagnostics expose relay URL, wire subscription count, authors served, lane counts, exact wire-filter JSON, events received by kind, and per-filter coverage intervals. The snapshot also exposes uncovered author count, dropped merge rules, and transport degradation on all supported tiers.

Each relay summary also carries the latest NIP-11 `supported_nips` advertisement, cited document revision, freshness, last refresh error, NIP-77 advertisement state, and an independently sourced behavioral NIP-77 state. Advertisement may influence whether a probe starts, but only a real NEG response creates behavioral proof; a document cannot mint that authority.

Rust additionally exposes discovered-private-relay rejection count, over-cap rejection count, `store_degraded`, and transport degradation. Swift/Kotlin do not currently project `store_degraded` or the two rejection counters. Do not design a native recovery screen around fields it cannot observe.

Do not claim that diagnostics currently provide:

- demand graph nodes or refcounts;
- write-intent/receipt queues, retry schedule, or write attempts;
- scheduler/queue-pressure telemetry;
- public pending-row signature state;
- a global connection generation or populated AUTH lifecycle;
- database row counts or GC telemetry.

Engine-start and observation infra failures are call facts, not diagnostics snapshot fields. Engine construction can return `EngineError::EngineStartFailed`; a live observe (including NIP-02 following observation) can return `EngineError::ObservationUnavailable` when a required relay connection cannot be opened. The follow action has no capacity or thread refusal and reports any genuine terminal failure as `FollowActionStatus::Failed` with a `FollowActionFailure` value, and initial direct-Rust NIP-46 setup returns the matching `Nip46Error`. Native post-handle streamed NIP-46 failures are separate again. Preserve the exact owning shape instead of waiting for diagnostics to explain an absent or closed stream.

NIP-11 service closure and acquisition failure are likewise one-shot call facts, not scheduler diagnostics. A successful stale snapshot can carry its refresh error while diagnostics retain the cited last-good advertisement; absence of behavioral proof remains absence.

There is no worker/task census, idle barrier, or task-capacity knob to poll: #704 removed internal task admission. Do not model queue pressure, expose task counts as product telemetry, or treat any capacity as a retry policy.

`SourceStatus.awaitingAuth`/`authDenied` and `AuthPhase` exist as reserved public vocabulary but are not populated by the current engine. Label them reserved if they appear in exhaustive UI switches.

For debugging, compare the query's semantic filter/evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Correlate by relay where available, but do not promise an exact public query-to-wire-filter join: `SourceEvidence` carries no stable filter id/JSON and Swift's encoder is internal. Preserve absence as absence: no coverage row is unproven, not zero or complete.
