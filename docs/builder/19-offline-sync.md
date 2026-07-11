# Offline, reconnect, and acquisition evidence

Offline behavior is not a special application mode. NMP serves its persistent
local replica, reports the evidence it has, and resumes still-declared demand
when sources become reachable.

## Cold reads are local facts

On construction, matching cached rows may be available before any relay
connects. A query snapshot distinguishes that local cache state from acquisition
against its currently planned sources.

An empty cached result means only that the local replica has no current match.
It does not mean no matching event exists on Nostr. A private, unknown, LAN,
slow, AUTH-gated, or offline relay may hold additional data.

Apps may present offline or partial-source UX from the reported facts. NMP does
not collapse them into `syncHealth`, `synced`, or global completeness.

## Watermarks are scoped evidence

EOSE or a completed reconciliation proves that one relay finished one request
shape for one window under one source/access context. Persisting that evidence
supports restart, incremental acquisition, diagnostics, and avoiding redundant
work.

It never proves that all matching events everywhere have been found.

A snapshot therefore carries compact status for every planned source, including
facts such as connecting, reconciled-through, disconnected, AUTH-required, or
failed, plus explicit shortfall when the plan itself could not cover the whole
demand. Exact wire filters, EOSE, errors, and watermarks remain available in
diagnostics.

Evidence changes may emit a new snapshot without a row change.

## Negentropy is capability-gated

NIP-77 reconciliation is used only after that relay has proved support. The
capability token has no app constructor; an unprobed or unsupported relay uses
ordinary REQ acquisition.

Limited filters also require care because relay-side limits do not compose
freely with set reconciliation or coverage attribution. NMP chooses the safe
wire mechanism; the app declares the selection.

## Reconnect restores demand, not app subscriptions

When a connection generation ends, NMP discards attribution tied to that
generation, reconnects according to transport policy, and recompiles/replays the
still-live wire demand with fresh attribution.

The app does not watch connection state and reopen REQs. Query ownership is the
source of truth for whether demand still exists.

An in-progress reconciliation is connection-local. A new connection starts the
appropriate fresh work for the same semantic demand; it does not pretend a
half-finished exchange continued unchanged.

## Write retry has a different owner

Socket reconnect and publication retry are separate mechanisms:

| Concern | Owner |
|---|---|
| Reconnect a socket | transport |
| Correlate one remote signer operation | signer provider |
| Retry one `(intent, relay)` publication lane | durable outbox |
| Wake eligible work and cap concurrency | engine deadline scheduler |

The transport must not hide a durable EVENT buffer below the outbox. For every
durable lane the outbox persists exact signed bytes and `AttemptStarted` before
dispatch, then records outcome, ordinal, and next eligibility.

Offline and AUTH-blocked time do not consume attempts. Transient failures advance
logical backoff. ACK and permanent rejection close their lane. At-most-once
ambiguity becomes `OutcomeUnknown` and is never blindly resent.

One deadline scheduler sleeps until actual work is eligible. There is no
fixed-rate polling or one timer thread per intent.

Explicitly non-durable writes do not resume their publication obligation after
process loss. Their minimal receipt remains reattachable and reports an explicit
terminal policy fact instead of disappearing.

## Process restart

The app reconstructs the engine and declares its query set again. NMP restores:

- the canonical local event replica and provenance;
- source evidence and watermarks;
- accepted pending rows and signer obligations;
- signed write lanes, attempts, and retry eligibility; and
- retained receipt facts.

Dropping a query owner before termination still means its demand is gone.
Restart does not invent app queries that no caller declared. Durable writes are
different: acceptance transfers the obligation to NMP until its explicit
terminal policy is reached.

See [Current implementation status](03-status-map.md) for which parts of this
contract ship today.

---

<!-- nav-footer -->
<sub>[Index](README.md) · [Capabilities](20-capabilities.md) →</sub>
