# Diagnostics

Use two different proof surfaces for two different questions:

- an observation frame's `evidence` answers what sources and shortfalls apply to one query's own subtree — `Frame.evidence` in Rust, `RowBatch.evidence` on Swift/Kotlin. It is a list, one entry per canonical query branch in branch order, so a union keeps each branch's facts separate and one branch's shortfall is never masked by a sibling's proof.
- the diagnostics stream describes the engine-global current relay plan and observed wire facts.

Current cross-platform relay diagnostics expose relay URL, `access` context, wire subscription count, authors served, lane counts, exact wire-filter JSON, events received by kind, and per-filter coverage intervals. A relay row's identity is the `(relay, access)` pair, not the URL alone. The snapshot also exposes `authSessions`, uncovered author count, dropped merge rules, transport degradation, `stalledWrites`, and `stalledWriteTotals` on all supported tiers.

Each relay summary also carries the latest NIP-11 `supported_nips` advertisement, the cited document revision, freshness, the last refresh error, and three separately sourced NIP-77 facts: what the document advertised, what behavior proved, and where the handoff currently sits. Advertisement may influence whether a probe starts, but only a real NEG response creates behavioral proof; a document cannot mint that authority — which is why the advertised and behavioral fields never collapse into one.

Freshness and the three NIP-77 facts are stringly typed on every tier, so match the exact tokens rather than inventing your own: freshness is `fresh` or `stale` (absent when unknown); `nip77Advertisement` is `unknown`, `advertised_supported`, or `advertised_unsupported`; `nip77Behavior` is `unknown`, `probing`, `behaviorally_proven`, or `behaviorally_rejected`; `nip77Handoff` is `none`, `awaiting_live_eose`, `reconciling`, `backfilling`, `fallback_backlog`, or `live`. Treat an unrecognized token as unknown, never as a failure.

Rust additionally exposes `sessions_rejected_over_cap`, `sessions_refused_by_subscription_budget`, `store_degraded`, and the relay-level `subscription_budget`, `subscriptions_refused`, `subid_length_limit`, and `subid_length_rejects_our_ids`. Only `sessions_rejected_over_cap` reaches raw UniFFI, and even that one stops there — no ergonomic wrapper carries it. `sessions_refused_by_subscription_budget`, `store_degraded`, and all four relay-level advertised-budget fields are direct-Rust only. Do not design a native recovery screen around fields it cannot observe.

Do not claim that diagnostics currently provide:

- demand graph nodes or refcounts;
- a retry schedule or per-attempt write rows (a bounded `stalledWrites` census does exist, keyed by stage `Unroutable`/`Unsignable`/`Undeliverable`, and it is not a receipt id);
- scheduler/queue-pressure telemetry;
- public pending-*row* signature state (write signature state is public, on `PublishQueueEntry.signing` and as the `Unsignable` stall stage);
- database-level demand refcounts;
- database row counts or GC telemetry.

Engine-start and observation infra failures are call facts, not diagnostics snapshot fields. Engine construction can return `EngineError::EngineStartFailed`; an ordinary or windowed observe can return `EngineError::ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening. Relay connection/worker failure is acquisition evidence, not this error. The follow action has no capacity or thread refusal and reports any genuine terminal failure as `FollowActionStatus::Failed` with a `FollowActionFailure` value. Preserve the exact owning shape instead of waiting for diagnostics to explain an absent or closed stream.

NIP-11 service closure and acquisition failure are likewise one-shot call facts on the `relayInformation` read, not scheduler diagnostics. The typed family is exactly `ServiceClosed`, `CredentialedRelayUrl`, `Http`, `ResponseTooLarge`, and `InvalidDocument` — a failure is always one of these values and never an empty relay document. A successful stale snapshot can carry its own `lastError` beside the last-good document while diagnostics retain the cited advertisement; absence of behavioral proof remains absence.

There is no worker/task census, idle barrier, public task-capacity knob, or worker saturation outcome to poll. Private NIP-11 bounds apply backpressure internally; do not model queue pressure, expose task counts as product telemetry, or treat a physical bound as an app retry policy. NMP does keep a process-wide OS-thread census, but it is doc-hidden falsifier instrumentation for proving that observations cost no thread and that teardown leaves no orphan — never a diagnostics field, never an app-facing number, and never available past the Rust tier.

`SourceStatus.awaitingAuth`/`authDenied` are live, populated states, filled from exact per-session AUTH bookkeeping for `AccessContext::Nip42` demands; a connected protected session with no entry yet reads as `AwaitingAuth { AwaitingChallenge }`. Only `Public` demands can never reach them.

Two AUTH phase vocabularies exist, and confusing them is the easy mistake here. Direct Rust keeps them as separate types. The scoped `AuthPhase` on `SourceStatus::AwaitingAuth` has exactly four members — `AwaitingChallenge`, `AwaitingPolicy`, `AwaitingSignature`, `AwaitingRelayAck` — and deliberately no completed or denied member, because an authenticated source is simply `Requesting` and `AuthDenied` is its own top-level status. The engine-global `AuthDiagnosticsPhase` on `DiagnosticsSnapshot.auth_sessions` has eight: those four plus `AwaitingSend`, `Ready`, `Denied`, and `Error`. Swift and Kotlin flatten both into one eight-case `AuthPhase` shared by evidence and diagnostics; every member reads the same on every tier, `awaitingSend` included (#1616). Only four of those cases can reach a `SourceStatus`: `awaitingSend` and `ready`/`denied`/`error` come from an `authSessions` row and nowhere else.

`AwaitingSend` and `AwaitingRelayAck` are not two names for "waiting". `AwaitingSend` is NMP's own pending work — the kind:22242 event is signed and transport has not taken it — while `AwaitingRelayAck` is the relay's. An app choosing between waiting, warning, and failing over acts on that difference.

`DiagnosticsSnapshot.authSessions` carries the per-session lifecycle on all three tiers: relay, access, transport generation, epoch sequence, the challenge descriptor, phase, policy/signer binding, and the frozen AUTH event id. Two names differ by tier — the descriptor is `challenge_hash` in Rust and `challengeDescriptor` on Swift/Kotlin — and `transport_slot` is direct-Rust only. There are no `sendHandoffAccepted`/`relayOKAccepted` booleans: the phase owns those facts outright ("transport took the event" is `awaitingRelayAck` or `ready`; "the relay's OK was correlated" is `ready`), and a second field restating a phase is a second field that can contradict it. Raw challenge bytes and capability-instance identities never cross any of these boundaries.

For debugging, compare the query's semantic filter/evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Correlate by relay where available, but do not promise an exact public query-to-wire-filter join: `SourceEvidence` carries no stable filter id/JSON and Swift's encoder is internal. Preserve absence as absence: no coverage row is unproven, not zero or complete.
