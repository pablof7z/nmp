# Routing and context testing

Routing defects often conflate contexts. Name the relevant axes and an
unrelated control.

| Axis | Distinctions |
|---|---|
| Demand | literal, reactive, derived dependency |
| Identity | active, explicit, frozen author, remote signer |
| Source | app-supplied, discovered, derived, cached |
| Relay role | read, write, indexer, host, delivery |
| Access | public, AUTH/account, session/capability epoch |
| Request | filters, range, limit, module context |
| Evidence | cached, requested, EOSE, unavailable, limited, shortfall |
| Bound | caller limit, engine cap, route cap |
| Lifecycle | initial, reconfigured, reconnecting, restarted |
| Write phase | accepted, awaiting signer, signed, attempted, outcome |

## Core rules

- Reroute only demand that depends on the changed input. Test a similar pinned
  control.
- Separate selected sources from actual contacts and outcomes. Use planner
  proofs, diagnostics, relay witnesses, and receipts for their own facts.
- Discovery begins without the derived route. Seed the protocol fact at the
  discovery root; observe ingestion, contact, and facade result.
- Scope evidence to the exact request, source, identity/AUTH/session context,
  range, limit, and terminal observation that earned it.
- One source's EOSE is not global completion. Empty rows are not proof of a
  request, availability, completeness, or absence.
- Separate caller limits, engine caps, exact chunking, and explicit shortfall.
- Do not borrow routes, evidence, or capability across identities, access
  contexts, or epochs without a documented rule.
- Distinguish write acceptance, signer wait, signature, attempt, relay outcome,
  and terminal ambiguity. Apply documented pinning after acceptance.

## Proof

- unit tests: normalization and local classification;
- property/model tests: dependencies, caps, admissibility, invariants;
- facade tests: discovery-to-result and context changes;
- relay witnesses: contact and non-contact;
- restart tests: durable routes, evidence, and writes;
- parity/native tests: FFI/platform context.

## Review

Ask:

- What does this demand depend on, and what must not affect it?
- Was each source supplied, discovered, or derived, and in which role?
- Under which identity, AUTH, session, request, and bound was evidence earned?
- Are rows, selection, contact, EOSE, shortfall, and completeness distinct?
- Did setup inject the route or result?
- Which facts survive restart or remain pinned?
