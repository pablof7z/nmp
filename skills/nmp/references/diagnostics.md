# Diagnostics

Use two different proof surfaces for two different questions:

- `RowBatch.evidence` answers what sources and shortfalls apply to one query subtree.
- the diagnostics stream describes the engine-global current relay plan and observed wire facts.

Current cross-platform relay diagnostics expose relay URL, `access` context, wire subscription count, authors served, lane counts, exact wire-filter JSON, events received by kind, and per-filter coverage intervals. A relay row's identity is the `(relay, access)` pair, not the URL alone. The snapshot also exposes `authSessions`, uncovered author count, dropped merge rules, transport degradation, `stalledWrites`, and `stalledWriteTotals` on all supported tiers.

Each relay summary also carries the latest NIP-11 `supported_nips` advertisement, cited document revision, freshness, last refresh error, the NIP-77 handoff state, and an independently sourced behavioral NIP-77 state. Advertisement may influence whether a probe starts, but only a real NEG response creates behavioral proof; a document cannot mint that authority.

Rust additionally exposes `sessions_rejected_over_cap`, `sessions_refused_by_subscription_budget`, `store_degraded`, and the relay-level `subscription_budget`, `subscriptions_refused`, `subid_length_limit`, and `subid_length_rejects_our_ids`. The session rejection counters reach raw UniFFI but not the ergonomic wrappers; `store_degraded` reaches neither and is direct-Rust only. Do not design a native recovery screen around fields it cannot observe.

Do not claim that diagnostics currently provide:

- demand graph nodes or refcounts;
- a retry schedule or per-attempt write rows (a bounded `stalledWrites` census does exist, keyed by stage `Unroutable`/`Unsignable`/`Undeliverable`, and it is not a receipt id);
- scheduler/queue-pressure telemetry;
- public pending-*row* signature state (write signature state is public, on `PublishQueueEntry.signing` and as the `Unsignable` stall stage);
- database-level demand refcounts;
- database row counts or GC telemetry.

Engine-start and observation infra failures are call facts, not diagnostics snapshot fields. Engine construction can return `EngineError::EngineStartFailed`; an ordinary or windowed observe can return `EngineError::ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening. Relay connection/worker failure is acquisition evidence, not this error. The follow action has no capacity or thread refusal and reports any genuine terminal failure as `FollowActionStatus::Failed` with a `FollowActionFailure` value. Preserve the exact owning shape instead of waiting for diagnostics to explain an absent or closed stream.

NIP-11 service closure and acquisition failure are likewise one-shot call facts, not scheduler diagnostics. A successful stale snapshot can carry its refresh error while diagnostics retain the cited last-good advertisement; absence of behavioral proof remains absence.

There is no worker/task census, idle barrier, public task-capacity knob, or worker saturation outcome to poll. Private NIP-11 bounds apply backpressure internally; do not model queue pressure, expose task counts as product telemetry, or treat a physical bound as an app retry policy.

`SourceStatus.awaitingAuth`/`authDenied` and `AuthPhase` are live, populated states, filled from exact per-session AUTH bookkeeping for `AccessContext::Nip42` demands; a connected protected session with no entry yet reads as `AwaitingAuth { AwaitingChallenge }`. Only `Public` demands can never reach them. `DiagnosticsSnapshot.authSessions` carries the matching per-session lifecycle — transport slot and generation, epoch sequence, challenge hash, phase, policy/signer binding, and handoff/ack acceptance — on all three tiers.

For debugging, compare the query's semantic filter/evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Correlate by relay where available, but do not promise an exact public query-to-wire-filter join: `SourceEvidence` carries no stable filter id/JSON and Swift's encoder is internal. Preserve absence as absence: no coverage row is unproven, not zero or complete.
