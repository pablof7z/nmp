# Routing and context testing

Routing bugs usually come from collapsing contexts that look similar at one layer but have different meaning. NMP tests must make those axes explicit.

Do not write “routing works.” State what is being routed, from which evidence, under which identity and source context, and what must remain unchanged.

## Start with the context axes

Before changing routing, acquisition, coverage, or source selection, enumerate the relevant axes.

| Axis | Common distinctions | Why it matters |
|---|---|---|
| Demand binding | literal, reactive input, derived dependency | Determines what reroots when inputs change |
| Identity selection | active account, explicit account, frozen author, remote signer | Determines whose sources/auth/signature apply |
| Source provenance | app-supplied, indexer-discovered, protocol-derived, cached | Determines authority and refresh behavior |
| Relay role | read relay, write relay, indexer, group host, delivery service | Roles are not interchangeable |
| Access context | public, NIP-42 authenticated, account-specific, session epoch | Same URL can represent different evidence domains |
| Query scope | filters, time/range, limit, module context | Evidence is scoped to the exact request |
| Freshness/evidence | cached rows, requested, EOSE, unavailable, limited, shortfall | Rows and acquisition status are separate facts |
| Boundedness | caller-requested bound, engine cap, route cap | A limit must not silently become completeness |
| Lifecycle | initial, reconfigured, reconnecting, restarted | Source and identity facts may be pinned or reconstructed |
| Write phase | accepted, awaiting signer, signed, attempted, ACKed/rejected | Each phase carries different truth and durability |

Not every change involves every axis. Explicitly rule out the irrelevant ones rather than assuming they are equivalent.

## Identify dependencies, not coincidences

The primary rerouting question is:

> Does this demand semantically depend on the changed input?

Examples:

- A query derived from `$currentPubkey` reroots when the active account changes.
- A query containing Alice's pubkey literally does not reroot merely because Bob becomes active.
- A write accepted with an explicit signer remains pinned to that signer even if the default account later changes.
- A source discovered for one account/auth context does not automatically become evidence for another.

Test both the changed dependency and an unrelated control. A positive reroute assertion without the pinned control is incomplete.

## Separate planning from execution

Routing has at least two externally meaningful questions:

1. Which sources are admissible and selected?
2. Which sources were actually contacted and what happened there?

Use different evidence:

- planner/property tests for source-set invariants;
- diagnostics for explainability;
- independent relay witnesses for actual contacts;
- receipts for write attempts and outcomes.

Do not treat a selected route as proof of network contact. Do not treat absence from diagnostics as proof that no contact occurred.

## Separate discovery from preconfiguration

A self-bootstrapping scenario must begin without the derived route.

Truthful sequence:

1. the app supplies only indexers or another discovery root;
2. the indexer holds the protocol fact that names the next source;
3. NMP requests and ingests that fact;
4. NMP contacts the discovered source;
5. the facade exposes the resulting rows and source evidence.

The fixture must not call a resolver/directory ingestion helper with the route before the scenario starts.

Required contrasting cases often include:

- discoverable source is found;
- absent relay-list fact yields explicit shortfall/unknown, not a fabricated empty completion;
- malformed or inadmissible source is rejected;
- changing one author's relay list reroutes only dependent demand;
- unrelated literal routes remain active.

## Evidence is scoped

Never attach acquisition evidence more broadly than the request that earned it.

At minimum, reason about:

- exact filter or descriptor;
- time/range scope;
- source/relay identity;
- account and AUTH context;
- relay session or capability epoch when relevant;
- limits and shortfall;
- terminal observation that minted the evidence.

A source reaching EOSE proves a fact about that source and request. It does not prove:

- every planned source completed;
- no other source exists;
- cached rows are globally complete;
- another account/auth context has the same evidence;
- a larger range is complete;
- a limited request exhausted the domain.

Avoid vague words such as “synced,” “fully synced,” or “authoritatively empty” unless the exact scoped evidence is stated.

## Rows and evidence are different outputs

An empty row set can coexist with:

- no request having been made;
- all sources unavailable;
- one source complete and another blocked;
- a caller-requested zero/small limit;
- an engine-imposed shortfall;
- a completed request that genuinely returned no matches.

Feature examples should distinguish the cases the product exposes. Tests must assert both data and acquisition/evidence facts where the distinction matters.

## Caller limits and engine shortfall

Do not silently turn bounded work into completeness.

Distinguish:

- **caller-requested bound:** the app asked for at most N results;
- **engine cap:** NMP limited route/filter/source expansion;
- **shortfall:** NMP could not satisfy the semantic request within its bound;
- **exact chunking:** NMP split the complete semantic request into bounded operations.

Property/model tests should cover broad combinations. Feature scenarios should preserve the product-visible distinction, not enumerate every cap value.

## Access and identity isolation

Treat the same relay URL under different access contexts as potentially different evidence domains.

Test isolation where applicable between:

- anonymous/public and NIP-42-authenticated access;
- authenticated accounts;
- signer identities;
- session/capability epochs;
- explicit and default identity selection;
- abandoned and active NIP-77 candidate requests.

No evidence, capability, or route should be borrowed across contexts unless a documented product rule permits it.

## Write routing truth

Write tests must distinguish phases instead of asserting a generic “sent” state:

1. **accepted** — NMP owns a durable obligation;
2. **awaiting signer** — no valid signature is available yet;
3. **signed** — exact author and body are frozen;
4. **attempted** — a route attempt occurred;
5. **ACKed/rejected** — a relay reported an outcome;
6. **cancelled/expired/ambiguous** — the obligation ended or cannot safely be retried.

Changing default account, routing facts, or relay availability after acceptance must obey the documented pinning rules. Add scenarios that contrast default selection with explicit overrides.

## Scenario-writing pattern

For context-sensitive routing, use this shape:

```gherkin
Rule: <one contextual axis changes the result>

  Scenario: <case that should change>
    Given <the dependency/context>
    And <an unrelated control is also active>
    When <the contextual input changes>
    Then <the dependent route/result changes>
    And <the unrelated control remains unchanged>

  Scenario: <similar case that must not change>
    Given <the literal/frozen/independent context>
    When <the same input changes>
    Then <the route/result remains pinned>
```

The paired examples preserve the boundary better than a generic happy path.

## Proof selection

Use:

- unit tests for normalization and local classification;
- property/model tests for dependency graphs, caps, admissibility, and invariants;
- facade integration tests for discovery-to-result and account-change behavior;
- external relay witnesses for actual contact/non-contact;
- restart tests when routes, evidence, or accepted writes claim durability;
- parity/native tests when the context crosses FFI or platform state.

## Routing review questions

Before finishing, answer:

- Which inputs does this demand actually depend on?
- Which similar inputs must not affect it?
- Was the source supplied, discovered, or derived?
- What is the source's role?
- Under whose account/AUTH/session context was the evidence earned?
- What exact request scope does the evidence cover?
- Are empty rows being confused with completed acquisition?
- Is a bound being confused with completeness?
- Did the fixture preload the route the product was meant to discover?
- Is actual network contact independently observed?
- What changes after restart, and what must remain pinned?
