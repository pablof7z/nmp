# Bug-class acceptance ledger

This ledger replaces governance-by-lint. A bug class is closed only when a
type/API mechanism makes the bad path unreachable and a falsifier demonstrates
that fact. `TARGET` records an agreed invariant whose mechanism is not built;
`PARTIAL` means current proof covers only part of the promoted contract.

| # | Bug class | Structural exclusion | Current proof status |
|---|---|---|---|
| 1 | **Stale replaceable event retained** | One canonical mutating store door performs exact-id dedup, then replaceable arbitration; no public index setter exists. | **BUILT / M3 verified.** Both stores exercise supersession through the door; retraction of the displaced row is tracked separately. |
| 2 | **Lost or leaked subscription** | Wire demand derives only from refcounted live-query descriptors; there is no public open-REQ verb. | **BUILT / M2 verified.** Surgical open/close and last-observer withdrawal have headless proofs. |
| 3 | **Wrong-relay routing through app relay injection** | Relay selection is compiler output from typed source/lane/context facts. There is no general raw `relays:` argument on observe/publish. | **BUILT for default routing; contextual protocol contributions are TARGET** (§14). |
| 4 | **Uncapped fan-out** | Every plan carries an explicit cap and reports uncovered demand; no relay outside the compiled plan is contacted. | **PARTIAL.** M2 proves the current solver cap; whole-demand/global cap and promoted shortfall contract remain to build. |
| 5 | **Dedup or provenance loss** | Duplicate id insertion merges provenance before other processing; event id/signature bytes remain canonical. | **BUILT / M3 verified** for both store backends and live duplicate delivery. |
| 6 | **Private-event republish** | Private/narrow route values have no widen operation; unresolved private routing fails closed. | **BUILT for current narrow route types; future protocol modules must preserve it.** |
| 7 | **Cache evidence presented as global truth** | Query snapshots expose rows plus per-current-plan source facts and explicit shortfalls, computed over every root and interior atom. No public query state expresses a global completeness verdict. | **BUILT for scoped evidence and the #12 interior-atom proof; PARTIAL for broader #49.** Rust/FFI/Swift/Kotlin carry `AcquisitionEvidence`, while full `SourceAuthority + AccessContext` descriptor identity and context-isolated evidence/coalescing remain TARGET. |
| 8 | **Assuming NIP-77 support** | Negentropy requires a capability token minted only after probe; unprobed relays use REQ. | **BUILT / M3 verified.** |
| 9 | **Accepted/enqueued treated as converged** | Durable `Accepted` is constructible only after atomic persistence of intent, receipt, frozen body, and canonical pending row. Delivery remains per-relay receipt evidence. | **PARTIAL.** Receipt streaming is live-proven; crash-safe acceptance and pending-row persistence are TARGET. |
| 10 | **Signer drift after acceptance** | Publish defaults to the signer for `$currentPubkey`, permits an explicit identity override, then freezes the selected expected author at `Accepted`. | **TARGET.** Current active-account/signer coupling and re-root proof do not exclude queued-write reassignment. |
| 11 | **App owning interest expansion** | `Derived`/`SetOp` resolve inside the engine; helpers return the same closed printable graph; no expanded-set callback exists. | **BUILT for the raw grammar.** Module-provided reusable fragments remain TARGET. |
| 12 | **Presentation or plaintext policy in core** | Engine/module values contain protocol-semantic raw data only. Formatting remains app/UI-owned; decrypted results, when supported, remain raw protocol values. | **PARTIAL.** Unencrypted raw-token boundary exists; decrypt return path and protocol modules are not complete. |
| 13 | **Late-arriving event skipped by presentation pagination** | Durable ingest/source cursor and ordered presentation cursor are distinct types; paging cannot advance acquisition. | **TARGET / candidate retained from Collection exploration.** |
| 14 | **Schema ownership confused with contextual publication authority** | A module claims only exact NIP-defined schemas. Contextual operations contribute closed typed tags/routes without claiming a foreign draft's kind. | **TARGET.** Existing ownership design incorrectly gates every route contribution on kind ownership. |
| 15 | **Pending write bypasses normal query semantics** | `Pending(intentId) | Signed(signature)` is state on the canonical store row; acceptance, promotion, cancellation, replacement, delete, and expiry all use the one store mutation path. | **TARGET.** No optimistic overlay or direct write-to-observer lane is admissible. |
| 16 | **Durable obligation lost or duplicated by retry layers** | Outbox is the sole persisted `(intent, relay)` attempt owner; transport owns sockets only; signer adapter owns one RPC; one deadline scheduler owns time. | **TARGET.** Persisted attempt ordinal/outcome/eligibility and restart proofs remain unbuilt. |
| 17 | **Silent truncation under limits** | Every graph/wire/relay/result limit either preserves exact semantics, rejects explicitly, or emits shortfall evidence. First-N masquerading as complete is unrepresentable. | **PARTIAL.** Swift newest-frame buffering and several router caps exist; end-to-end limit evidence does not. |
| 18 | **Demand/evidence conflated across source or access context** | Descriptor identity is `selection + source authority + access context`; selection may share internally, but wire/evidence shares only after compatibility proof. | **TARGET.** Current public query identity is effectively filter-only. |
| 19 | **Secret material made part of durable event storage** | Rust persistence stores obligations and expected pubkeys, never raw signing secrets. Standard platform providers hold secrets behind secure-storage capability boundaries. | **TARGET.** Local signer exists; provider/vault boundary and reattachment are not built. |

Every status must remain honest. A design document, issue, or passing adjacent
test is not proof. Promoting `TARGET`/`PARTIAL` to `BUILT` requires a falsifier
against the supported public facade and, where applicable, restart and platform
projection tests.
