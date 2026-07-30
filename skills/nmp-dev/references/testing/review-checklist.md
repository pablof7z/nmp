# Testing review checklist

Use this checklist for behavioral changes, test additions, scenario edits, and acceptance-harness work.

## Behavioral memory

- [ ] Did the user introduce a correction, contextual distinction, counterexample, forbidden consequence, or durability rule?
- [ ] Is that nuance preserved in the canonical feature corpus?
- [ ] Does the feature live under the behavioral domain rather than the edited crate?
- [ ] Was wrong text corrected in place instead of contradicted by a new appendix or feature?
- [ ] Do contrastive scenarios show both what changes and what must remain unchanged?
- [ ] Does each scenario have one clear promise?
- [ ] Is the prose free of unnecessary private Rust/table/helper vocabulary?

## Metadata and status

- [ ] Is `nmp:id` unique and stable?
- [ ] Is status exactly `built`, `specified`, or `known-violation`?
- [ ] Does every `built` scenario have evidence and a falsifier?
- [ ] Does every `specified` scenario name the exact gap and issue?
- [ ] Does every `known-violation` scenario link the repair issue?
- [ ] Is every `@acceptance` scenario `built`?
- [ ] Are evidence locators greppable and current?

## Evidence quality

- [ ] Is the proof located with the narrowest stable contract that owns the guarantee?
- [ ] Does the proof distinguish the scenario from its contrasting cases?
- [ ] Did the test fail before the fix or under the stated mechanism-disable falsifier?
- [ ] Could the named defect still exist while the test stays green?
- [ ] Is a broad state space handled by property/model testing rather than manual examples?
- [ ] Is there a facade capstone where the cross-component public consequence matters?
- [ ] Are platform/native proofs present when the contract crosses FFI or platform lifecycle?

## Fixture truthfulness

- [ ] Does setup stage real inputs rather than the derived result?
- [ ] For discovery, is the route absent initially and learned from the intended protocol fact?
- [ ] For durability, is state destroyed and reopened rather than merely read from the database?
- [ ] For network effects, is there an independent relay/provider witness where practical?
- [ ] Does the test avoid using a private handle to drive or observe a public facade claim?
- [ ] Are exact binary/build, identity, environment, and fixture ownership controlled?

## Routing and evidence context

- [ ] Are literal, reactive, and derived demand distinguished?
- [ ] Are explicit and default/frozen identities distinguished?
- [ ] Are source provenance and relay roles explicit?
- [ ] Are account, AUTH, session/capability epoch, filter/range, and limit scopes considered?
- [ ] Is one source's EOSE kept separate from global completion?
- [ ] Are empty rows kept separate from unknown, unavailable, limited, or shortfall states?
- [ ] Are caller limits kept separate from engine-imposed caps?
- [ ] Are selected sources kept separate from actual contacts and outcomes?

## Distributed behavior

- [ ] Is the claim classified as safety, liveness, durability, isolation, truthfulness, boundedness, or idempotence?
- [ ] Are relevant crash points, reordering, duplication, reconnect, and stale-response schedules tested?
- [ ] Is retry behavior safe under ACK ambiguity?
- [ ] Are waits bounded by observable conditions rather than arbitrary sleeps?
- [ ] Are randomized schedules replayable by seed and operation log?
- [ ] Are failure artifacts retained and secrets scrubbed?
- [ ] Does teardown prove no process, thread, port, file, or environment leakage?

## Test architecture

- [ ] Is the feature corpus acting as behavioral memory rather than a duplicate project plan?
- [ ] Is GitHub still the owner of temporary implementation work?
- [ ] Are mechanism tests kept in their owning crates?
- [ ] Does the acceptance layer primarily depend on `nmp`, `nmp-test-support`, and external witnesses?
- [ ] Are direct mechanism dependencies narrowly justified rather than used to bypass the facade?
- [ ] Has redundant shell/Cucumber/Rust evidence been removed after equivalent coverage is proven?
- [ ] Does CI expose acceptance, fault/persistence, parity/native, and live-probe roles clearly?

## Final question

- [ ] Would a future agent reading only the feature rule, scenarios, and evidence pointers understand the exact nuance the user established and know how to avoid reintroducing the wrong behavior?
