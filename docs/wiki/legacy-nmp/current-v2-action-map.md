# Legacy NMP to current v2 action map

Status: forensic migration map, not current architecture authority.

Current behavior is defined by `README.md`, `docs/VISION.md`,
`docs/design-record.md`, the focused current designs, `docs/bug-class-ledger.md`,
`docs/known-gaps.md`, current code/tests, and live GitHub issues. This map only
states where recovered legacy teachings land after comparison with those
authorities.

## Source-complete result

The frozen legacy knowledge bank contains 3,189 canonical records. The verified
partition is:

| Durable/source lane | Records | Review unit |
|---|---:|---|
| Atomic GitHub issues #244-#409 | 345 | 166 coherent issue contracts |
| Clarification buckets #412-#437 | 699 | Every source ID reviewed in raw context |
| Historical/context records from PR #438 | 249 | Every record reviewed; 24 referential records resolved from raw context |
| Previously local-only source records | 1,896 | Every source ID classified exactly once |
| **Total** | **3,189** | Zero missing, extra, or overlapping IDs |

The 26 clarification buckets contain 737 placements for 699 IDs because 38
update/event-driven records were placed in two buckets. Bucket placements are
therefore not a source count.

## Recovered current invariants

These teachings already match current v2 and should remain represented by the
current authority rather than by legacy API names:

- NMP is an embeddable sync-and-routing engine, not an application framework.
- The app-facing product nouns are a closed, introspectable live query and a
  durable write intent. Diagnostics is a read-only proof surface.
- Query identity includes selection, source authority, and access context.
  Returned rows prove only the selected sources and evidence, never global
  network absence.
- One canonical store and row path owns verified relay events, accepted local
  writes, replacement/deletion/expiration effects, and reactive invalidation.
- Accepted writes freeze body and signer identity, persist before attempting
  network work, use durable per-relay lanes, and expose factual reattachable
  receipts.
- Routing is compiler output from typed facts. Application relays are additive
  for default-routed operations; explicit private or host-authoritative protocol
  contexts may narrow that route.
- Opt-in protocol modules own exact schemas and semantic operations. A module
  may add its typed context to a foreign draft without claiming the draft kind.
- Product state, architecture, identity UX, formatting, moderation, navigation,
  and presentation ordering remain application-owned.
- Swift and Kotlin project the same semantic facade through native observation
  and cancellation patterns. Platform ergonomics must not create a second
  semantic owner.
- Relay ingestion is event-driven. Deadlines coalesce delivery and drive
  reconnect/backoff/maintenance; polling and the legacy 4 Hz full-snapshot
  mechanism are not current contracts.
- Durable verified event history is retained by default. RAM may be bounded;
  deliberate durable eviction must be explicit policy and must atomically lower
  the coverage it invalidates.

## Existing current owners

| Current owner | Recovered work that belongs there |
|---|---|
| #22, #45, #13 | Default author-outbox + eligible recipient-inbox + application-relay union; conditional fallback; relay hints on reads and referenced writes; typed route provenance; indexer write-back; draft/wiki relay authority; NIP-17 kind:10050 and other private/contextual routing. |
| #205, #10 | Explicit NIP-77 eligibility decision table; capability evidence; ids/address filters; one-shot behavior; realistic public-relay, follow-graph, and restart proof. The literal historical input `3*20 >= 50 -> negentropy` is evidence, not automatically the final v2 rule. |
| #45 | Exact protocol schema ownership and immutable contextual composition. Resolve the current mismatch where generic Rust composition exists while a kind:9-specific NIP-29 constructor/native projection remains privileged. |
| #47, #8, #40 | Per-write signer override; secure Keychain/Keystore restoration; Android NIP-55 execution/AAR proof; exact NIP-42 trust, prompt, denial, and multi-account policy. |
| #63 | Active-account NIP-51 kind:10009 remembered-group state composed into typed NIP-29 group/host selection and safe replacement writes. |
| #75, #155, #165 | Compose parity, broader kind renderers, live NIP-25 reaction resources/actions, source-installable component updates, scoped observation lifetime, and explicit media/render policy without moving product UX into Core. |
| #215, #422, #48, #63 | Direct entity and NIP-05 resolution, NIP-50 relay acquisition, optional local full-text indexing, protocol-scoped helpers, result evidence/ranking, and app-owned presentation. |
| #217, #423 | App-controlled WoT policy separated from acquisition authority. Decide whether reusable scoring is an optional pure local module or application code. Preserve the historical `no NIP-85 for now` boundary until newer evidence replaces it. |
| #219, #425 | Current wallet scope, per-account NWC, wallet-state explainability, recipient kind:10019 retrieval before terminal failure, and mint failover. These requirements do not by themselves assign all wallet product policy to Core. |
| #10, #176, #223 | Real relay/device/restart/performance falsifiers, including relay-to-render latency and the concrete legacy 60-90 second resync failure, without promoting illustrative measurements into universal constants. |
| #9 | Use the Mixtape probe to decide whether closed protocol ordering/window mechanics are sufficient while app-semantic ordering remains in the view model. Do not restore app comparators or the legacy feed framework. |

## Focused successor corrections

- Rewrite forensic #393 as the current NIP-01 ephemeral live-only observation
  contract: no persistence, ordinary ownership/cancellation, bounded delivery
  and backpressure, explicit evidence/diagnostics, and Rust/Swift/Kotlin parity.
- Split #303 into an NMP-owned typed kind:0 create/edit operation and
  application-owned signup/profile-screen behavior.
- #459 makes default durable-cache retention and its no-silent-eviction
  falsifier explicit in current store authority.
- #458 states in contributor/governance guidance that execution plans are
  temporal work artifacts, not architecture that becomes durable by inertia.
- Translate old Blossom, WASM, zap, and wallet `v1`/`post-v1` labels into current
  v2 milestones rather than preserving obsolete release names.
- Add or locate one repeatable end-to-end latency/count falsifier spanning relay
  burst, Rust ingestion/emission, FFI callback, native-main-thread application,
  and visible result.

## Archive or reject

The following do not become current NMP work merely because they appear in the
legacy corpus:

- `nmp-feed`, `FeedParams`, projection registries, read sessions, Trellis public
  APIs, FlatBuffers full-snapshot cadence, generated ownership tokens, and
  lint-only architecture enforcement;
- old Chirp, Highlighter, 29er, gallery, TUI, TestFlight, packaging, branch,
  agent-orchestration, and LOC-audit incidents, except where a current generic
  falsifier independently owns the same failure class;
- MLS/Marmot implementation instructions that were explicitly deferred;
- pasted agent recommendations without independent direct-human adoption; and
- unsupported broadenings such as a blanket ban on broad queries, cache
  presence suppressing acquisition, blind kind:10019 delivery, universal
  real-pubkey fixture rules, or reversed platform-parity requirements.

## Tracker completion rule

Forensic issues are coverage records, not implementation claims. A forensic
atomic may close once this catalog gives every source ID an explicit current
owner or explicit non-issue disposition. Current implementation issues remain
open until their own acceptance evidence passes. Root #200 may close only when
the catalog verifier passes and the live tracker links to the catalog without
claiming that recovered product work has already shipped.
