# Diagnostics

Use two different proof surfaces for two different questions:

- an observation frame's `evidence` answers what sources and shortfalls apply to one query's own subtree — `Frame.evidence`. It is a list, one entry per canonical query branch in branch order, so a union keeps each branch's facts separate and one branch's shortfall is never masked by a sibling's proof.
- the diagnostics stream describes the engine-global current relay plan and observed wire facts.

Relay diagnostics expose relay URL, `access` context, wire subscription count, authors served, lane counts, exact wire-filter JSON, events received by kind, and per-filter coverage intervals. A relay row's identity is the `(relay, access)` pair, not the URL alone. The snapshot also exposes `auth_sessions`, uncovered author count, dropped merge rules, transport degradation, `stalled_writes`, and `stalled_write_totals`.

Each relay summary also carries the latest NIP-11 `supported_nips` advertisement, the cited document revision, freshness, and the last refresh error.

Freshness is stringly typed, so match the exact tokens rather than inventing your own: `fresh` or `stale`, absent when unknown. Treat an unrecognized token as unknown, never as a failure.

The snapshot additionally exposes `sessions_rejected_over_cap`, `sessions_refused_by_subscription_budget`, and the relay-level `subscription_budget`, `subscriptions_refused`, `subid_length_limit`, and `subid_length_rejects_our_ids`. `sessions_refused_by_subscription_budget` is kept apart from `sessions_rejected_over_cap`: one says the plan was too wide for the operator's ceiling, the other says this relay will hold nothing open. Do not design a recovery screen around fields the snapshot does not carry.

There is no local-store health field, and there is no store-recovery screen to build. A durable-store operation that fails returns an opaque error and the engine carries on with the same handle: no degraded flag, no fault kind, no reopen to wait for, nothing for an app to branch on or display. `transport_degraded` is about relay transport, not local disk, and is the only degradation string here.

Do not claim that diagnostics currently provide:

- demand graph nodes or refcounts;
- a retry schedule or per-attempt write rows (a bounded `stalled_writes` census does exist, keyed by stage `Unroutable`/`Unsignable`/`Undeliverable`, and it is not a receipt id);
- scheduler/queue-pressure telemetry;
- public pending-*row* signature state (write signature state is public, on `PublishQueueEntry.signing` and as the `Unsignable` stall stage);
- database-level demand refcounts;
- database row counts or GC telemetry.

Engine-start and observation infra failures are call facts, not diagnostics snapshot fields. Engine construction can return `EngineError::EngineStartFailed`; an ordinary or windowed observe can return `EngineError::ObservationUnavailable` only when store degradation prevents its initial canonical projection from opening. Relay connection/worker failure is acquisition evidence, not this error. The follow action has no capacity or thread refusal and reports any genuine terminal failure as `FollowActionStatus::Failed` with a `FollowActionFailure` value. Preserve the exact owning shape instead of waiting for diagnostics to explain an absent or closed stream.

NIP-11 service closure and acquisition failure are likewise one-shot call facts on the `relayInformation` read, not scheduler diagnostics. The typed family is exactly `ServiceClosed`, `CredentialedRelayUrl`, `Http`, `ResponseTooLarge`, and `InvalidDocument` — a failure is always one of these values and never an empty relay document. A successful stale snapshot can carry its own `lastError` beside the last-good document while diagnostics retain the cited advertisement; absence of behavioral proof remains absence.

There is no worker/task census, idle barrier, public task-capacity knob, or worker saturation outcome to poll. Private NIP-11 bounds apply backpressure internally; do not model queue pressure, expose task counts as product telemetry, or treat a physical bound as an app retry policy. NMP does keep a process-wide OS-thread census, but it is doc-hidden falsifier instrumentation for proving that observations cost no thread and that teardown leaves no orphan — never a diagnostics field and never an app-facing number.

`SourceStatus::AwaitingAuth`/`AuthDenied` are declared states, populated from exact per-session AUTH bookkeeping only once a challenge has been received and reduced for demands naming an `authenticate_as` identity; a connected protected session with no entry yet reads as `AwaitingAuth { AwaitingChallenge }`. Both the write path and the read path reach them in the ordinary course, against a relay that challenges unsolicited on connect and against one that challenges only in response to a request — the read transmits first either way (#1889).

Two AUTH phase vocabularies exist, and confusing them is the easy mistake here. Rust keeps them as separate types. The scoped `AuthPhase` on `SourceStatus::AwaitingAuth` has exactly four members — `AwaitingChallenge`, `AwaitingPolicy`, `AwaitingSignature`, `AwaitingRelayAck` — and deliberately no completed or denied member, because an authenticated source is simply `Requesting` and `AuthDenied` is its own top-level status. The engine-global `AuthDiagnosticsPhase` on `DiagnosticsSnapshot.auth_sessions` has eight: those four plus `AwaitingSend`, `Ready`, `Denied`, and `Error`.

`AwaitingSend` and `AwaitingRelayAck` are not two names for "waiting". `AwaitingSend` is NMP's own pending work — the kind:22242 event is signed and transport has not taken it — while `AwaitingRelayAck` is the relay's. An app choosing between waiting, warning, and failing over acts on that difference.

`DiagnosticsSnapshot.auth_sessions` carries the per-session lifecycle: relay, access, transport generation, epoch sequence, the challenge descriptor (`challenge_hash`, a stable BLAKE3 descriptor), phase, policy/signer binding, and the frozen AUTH event id. There are no `sendHandoffAccepted`/`relayOKAccepted` booleans: the phase owns those facts outright ("transport took the event" is `AwaitingRelayAck` or `Ready`; "the relay's OK was correlated" is `Ready`), and a second field restating a phase is a second field that can contradict it. Raw challenge bytes and capability-instance identities never cross the facade.

For debugging, compare the query's semantic filter/evidence with diagnostics' exact wire JSON, lane, relay, event counts, coverage, and explicit local-limit shortfall. Correlate by relay where available, but do not promise an exact public query-to-wire-filter join: `SourceEvidence` carries no stable filter id/JSON. Preserve absence as absence: no coverage row is unproven, not zero or complete.
