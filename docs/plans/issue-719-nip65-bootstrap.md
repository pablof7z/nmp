# Issue 719 NIP-65 bootstrap plan

## Current-state gate

- NMP revision inspected: `d2004e00c3109e0d98f0eb2e692db0cdb7c659f1`
- Existing module/crate that overlaps: no NIP-65 protocol owner exists; core
  already parses network-ingested kind:10002 winners into the relay directory.
- Current public support: a direct Rust app can register and activate a signer,
  create ordinary write intents, and observe tracked receipts.
- Target-only mechanism or missing prerequisite: a typed, protocol-owned route
  for the first kind:10002 publication.
- Native packaging today versus target: this unit ships the direct-Rust
  `nmp-nip65` crate. FFI, Swift, and Kotlin do not expose the bootstrap
  operation in this unit and must not imitate it with raw relay arrays.
- Why an ordinary app `WriteIntent` is insufficient: `AuthorOutbox` correctly
  fails closed before the account's first kind:10002 is ingested, while the
  exact-route authority types are intentionally absent from the `nmp` facade.

## Exact ownership

- Owned schemas/kinds/tags: NIP-65 kind:10002 and its `r` rows with the
  unmarked/read/write marker vocabulary.
- Explicitly unowned participating content: subsequent kind:0, kind:3, notes,
  articles, and every other author write.
- Foreign draft/parser dependency: none.
- App-owned presentation/policy: relay selection, account UX, and how receipt
  facts are explained.

## Closed values and authority

- Public semantic inputs: an author, a non-empty bounded exact bootstrap relay
  set, a bounded NIP-65 relay list containing at least one write-capable relay,
  and an optional crash-safe correlation token.
- Returned operation: one-step publish returning the ordinary tracked receipt.
- Source/access/routing context: a dedicated bootstrap route authority whose
  relay set is immutable and whose constructor is withheld from the `nmp`
  facade.
- Why authority cannot be widened or forged: the protocol crate validates the
  whole set before minting the internal authority; the public operation exposes
  no raw `WriteIntent`, route constructor, directory writer, or transport.

## Read path

- Canonical demand: an ordinary live query for kind:10002 by the author.
- State reconstruction: the existing canonical store chooses the replaceable
  winner; the existing network-ingest path alone parses it into directory facts.
- Evidence exposed: ordinary rows/source provenance and ordinary write receipt
  facts.
- Observation teardown: ordinary `Subscription` drop/cancel.

## Write path

- Immutable composition stages: validate request, build exact NIP-65 tags,
  build an unsigned kind:10002, mint the exact bootstrap route, then hand the
  intent to `Engine::publish_tracked`.
- Module-owned validation failures: empty/oversized/duplicate bootstrap set,
  empty/oversized/duplicate relay-list entries, and no write-capable entry.
- Core acceptance/signing/outbox path: durable acceptance, active-account
  author check, registered signer, one signature, persisted exact route, normal
  relay attempts.
- Receipt/restart behavior: the returned `ReceiptStream` has the stable receipt
  id; the dedicated route snapshot round-trips across restart.

## Projection

| Tier | Public semantic surface | Ownership/teardown | Current or target |
|---|---|---|---|
| Rust | `publish_relay_list_bootstrap` | tracked receipt; app drains it | current |
| FFI | none | must not expose raw exact routes | explicit gap |
| Swift | none | must not bypass NMP with sockets | explicit gap |
| Kotlin | none | must not bypass NMP with sockets | explicit gap |

## Governance and falsifiers

- Supported-surface/doc changes: README protocol-module inventory, known gaps,
  supported-surface statement, surface change log if the facade snapshot moves.
- Persistence/diagnostics impact: one new durable route-snapshot variant; no
  directory/store mutation API and no synthetic observation fact.
- Collision/unowned-schema proof: workspace audit enrolls the exclusive
  kind:10002 claim with discovery acknowledgment.
- Unforgeable-authority proof: an external consumer compiles with only `nmp`
  and `nmp-nip65`; no `nmp-grammar` dependency.
- Deterministic composition/one-signature proof: fixed-time private seam tests
  exact bytes; the public integration uses the ordinary signer.
- Cross-platform parity proof: deliberately not claimed; the unsupported native
  projections are documented.
- Fixed clock/fixture for time-derived bytes: production samples current time
  in Rust; tests call a private fixed-time seam.
- Disabled-module/raw-engine proof: `nmp` does not depend on `nmp-nip65`.
- Bounded teardown/pressure-refusal proof: bootstrap relay and advertised relay
  counts are finite at construction; ordinary receipt/engine teardown applies.
