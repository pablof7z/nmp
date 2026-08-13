# Testing review checklist

Use only relevant sections.

## Meaning

- [ ] User corrections and contrasts are in the owning feature/`Rule`.
- [ ] Each scenario states one product promise using app or Nostr terms.
- [ ] Status and metadata match reality. Unfinished work links one open issue.
- [ ] Requested behavior runs normally without an unrequested runtime flag.

## Evidence

- [ ] Each executable check sits with the smallest stable component responsible
      for the behavior.
- [ ] Reintroducing the named bug or removing its protection makes the linked
      evidence fail.
- [ ] Rules across many inputs or operation orders use property or model tests.
- [ ] Public-API and native-platform tests are kept only when they prove an
      additional result.
- [ ] Every non-live evidence locator names one enabled test or validation
      script in required CI.
- [ ] Every live locator names one enabled supplemental live job.

## Fixtures

- [ ] Setup provides the inputs instead of inserting the expected result.
- [ ] Public claims use public API output or an independent witness, not private
      state.
- [ ] The test controls its build, identities, environment, clock, ports, and
      processes, then proves cleanup.

## Request context and distributed behavior

- [ ] The test says whether the request was given directly, follows changing
      state, or was produced from another request.
- [ ] The test states whose request it is and where each source came from.
- [ ] The job of each relay is clear: read, write, index, host, or deliver.
- [ ] Public or authenticated access, the account, and the session are explicit.
- [ ] The exact filter and range that produced each observation are clear.
- [ ] Caller, engine, and route limits remain separate, including the size of
      each request chunk and any known missing results.
- [ ] Reconfiguration, reconnection, and restart affect only the requests that
      depend on them.
- [ ] Cached data, requested responses, EOSE, unavailable sources, limits, and
      known missing results belong only to the request that produced them.
- [ ] Selecting a relay, contacting it, receiving rows, and receiving EOSE are
      tested as different facts.
- [ ] One relay finishing does not mark the whole request complete.
- [ ] For writes, acceptance, signer wait, signature, delivery attempt, relay
      outcome, and unresolved outcome are tested as different facts.
- [ ] Lost acknowledgements preserve uncertainty durably when the outcome
      cannot be known.
- [ ] The test covers every relevant restart, crash, operation order, duplicate,
      reconnect, and stale response.
- [ ] Failure output records a replay command and removes secrets.

## Architecture and completion

- [ ] Feature files describe behavior, issues track unfinished work, design docs
      explain architecture, and tests provide proof. None duplicates another.
- [ ] Tests use existing targets when possible. Redundant harnesses and duplicate
      tests or evidence locators are removed.
- [ ] The focused check, changed-crate tests, and required public-API or fault
      checks pass.
- [ ] Required parity and native-platform tests pass.
- [ ] Required repository checks pass.
- [ ] Unrun live or platform checks are stated.
- [ ] A future agent can recover the exact distinction, linked test, and
      deliberate-break check from feature metadata without reading chat.
