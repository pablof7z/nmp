# Routing and context testing

Routing bugs often happen when facts from one request are reused for another.
Before writing a test, record which details can change the result. Include a
similar request that should remain unchanged.

Ask:

- Was the request given directly, tied to changing state, or produced from
  another request?
- Which identity applies: the active account, an explicitly named author, an
  author fixed earlier, or a remote signer?
- How was the source obtained: supplied by the app, discovered, derived from
  other protocol data, or read from cache?
- What job does each relay have: read, write, index, host, or deliver?
- Was the request public, or did a relay verify an account through NIP-42 AUTH?
- Which account and session supplied that access? If permissions changed later,
  are results from the old access state kept separate?
- What exact filters, range, limit, and NMP module define the request?
- What did NMP actually observe: cached data, a requested response, EOSE, an
  unavailable relay, a limit, or known missing results caused by that limit?
- Which limit applies: one chosen by the caller, the engine, or the route?
- Is this the initial request, or has NMP been reconfigured, disconnected,
  reconnected, or restarted?
- For a write, has NMP only accepted it, asked the signer, received a signature,
  attempted delivery, or received a relay outcome?

## Core rules

- When an input changes, rerun only requests that depend on it. Include a
  similar fixed request to prove that unrelated work did not rerun or change.
- Choosing a relay does not prove NMP contacted it. Contacting a relay does not
  prove it replied. Use planner output for selection, relay logs for contact,
  and public results or receipts for outcomes.
- A discovery test must begin without the route it is supposed to find. Put the
  protocol fact where discovery starts, let NMP ingest it, then verify the relay
  contact and public result.
- Every observation belongs only to the request that produced it. This includes
  cached data, requested responses, EOSE, unavailable relays, limits, and known
  missing results. Do not reuse any of this evidence for a different source,
  identity, AUTH state, session, filter, range, or limit.
- Record how the request ended: for example, a relay sent EOSE, a deadline
  expired, or NMP recorded an unavailable source or limit. Evidence without its
  ending observation does not prove that the request stage completed.
- EOSE means one relay says it has finished the current subscription. It does
  not mean every relay finished. Empty rows do not prove that NMP made a
  request, that a relay was available, that the result is complete, or that no
  matching events exist.
- Keep caller limits, engine limits, route limits, and the size of each chunk
  separate. Report when a limit prevents a complete result.
- Do not reuse a route, evidence, or capability for another identity, access
  state, session, or later permission state unless a documented rule says they
  are equivalent.
- For writes, keep acceptance, signer wait, signature, delivery attempt, relay
  outcome, and unresolved outcome separate. After NMP accepts a write, later
  account or route changes must not silently change it unless a documented rule
  says otherwise.

## Proof

- Use unit tests for normalization and decisions made inside one component.
- Use property or model tests for dependencies, limits, allowed inputs, and
  rules that must hold across many cases.
- Use public-API tests for the path from discovery to result and for changes in
  request context.
- Use relay logs to prove contact or non-contact.
- Use restart tests for routes, evidence, and writes that must survive.
- Use parity and native tests for FFI and platform behavior.

## Review

Ask:

- What does this request depend on, and what must not affect it?
- Where did each source come from, and what job does it have?
- Which identity, AUTH state, session, filter, and limit produced this result?
- Does the test separately prove selection, contact, response, EOSE, limits,
  and completion?
- Did test setup insert the route or result that it claims to prove?
- Which facts must survive restart or remain fixed after acceptance?
