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

Executor saturation and OS-thread refusal are call/action facts, not diagnostics snapshot fields. Ordinary direct-Rust engine/query setup can return `EngineError::ThreadUnavailable`; NIP-02 observation additionally reserves a native task and can return `EngineError::ExecutorSaturated`. NIP-02 action-worker refusal is a terminal `FollowActionStatus::Failed` with the matching failure value, and initial direct-Rust NIP-46 setup returns the matching `Nip46Error`. Native synchronous outer-bridge errors and post-handle streamed NIP-46 failures are separate again. Preserve the exact owning shape instead of waiting for diagnostics to explain an absent or closed stream.

The raw FFI native-task census and exact idle barrier are lifecycle-test seams, not engine diagnostics. Swift/Kotlin keep their wrapper methods internal. Do not poll the census as queue pressure, expose it as product telemetry, or infer that increasing `maxNativeTasks` is a retry policy.

`SourceStatus.awaitingAuth`/`authDenied` and `AuthPhase` exist as reserved public vocabulary but are not populated by the current engine. Label them reserved if they appear in exhaustive UI switches.

For debugging, compare the query's semantic filter/evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Correlate by relay where available, but do not promise an exact public query-to-wire-filter join: `SourceEvidence` carries no stable filter id/JSON and Swift's encoder is internal. Preserve absence as absence: no coverage row is unproven, not zero or complete.
