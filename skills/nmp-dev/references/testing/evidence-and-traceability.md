# Evidence and traceability

Every `built` scenario needs evidence that exercises its mechanism, distinguishes
its contrasts, runs in required CI, and fails for the named defect. Nearby green
tests do not count.

## Locators

Use one line per load-bearing proof:

```text
# nmp:evidence=<kind>:<owner>::<target>
```

Kinds: `rust`, `swift`, `kotlin`, `parity`, `script`, `live`.

Examples:

```text
# nmp:evidence=rust:nmp-router::literal_demand_remains_pinned
# nmp:evidence=swift:NmpTests::receipt_reconstructs_after_restart
# nmp:evidence=script:repository::scripts/check-schema-ownership.sh
```

Use stable test names, not line numbers. A locator must resolve uniquely to an
enabled executable proof in a required lane. Ignored, conditionally compiled,
manual-only, masked, ambiguous, symlink-backed, or lookalike proofs fail.
`live` may supplement deterministic evidence, never replace it.

List each load-bearing layer. One model/property test may map to several
scenarios when it truly distinguishes them; do not clone tests for bookkeeping.

## Falsifiers

Every `built` scenario names the mutation or defect that should turn evidence
red:

```text
# nmp:falsifier=Treat one relay's EOSE as global completion; this scenario fails.
```

For new or changed evidence:

1. Disable, bypass, or invert the named mechanism.
2. Confirm the proof fails for the intended reason.
3. Restore it and confirm green.

If evidence stays green, it proves another claim.

## Strong observables

Prefer witnesses outside the mechanism:

- relay or signer logs for external effects;
- reopen through the facade for durability;
- typed consumer frames and receipts;
- parity/native bytes for platform claims.

Diagnostics explain behavior but should not self-certify an external effect
when an independent witness is practical.

## Status

Promote to `built` only when implementation, mapped evidence, the falsifier,
required restart/platform/failure dimensions, and all locators pass.

Use `specified` for an agreed `implementation`, `evidence`, `fixture`, or
`platform` gap. Use `known-violation` when current code contradicts the intended
contract. Both require a readable open issue; missing or closed issue state
fails traceability.

## Acceptance

An `@acceptance` scenario must:

- be `built` with a `rust:nmp::<test>` proof;
- drive and observe the supported facade or an external witness;
- use controlled setup without performing the behavior under test;
- use the exact build;
- prove cleanup.

Mechanism dependencies are fixture-only and need a narrow reason. The
transitional `nmp-bdd` runner is not facade acceptance evidence.

`tools/behavior-traceability` validates metadata, unique IDs, open issue state,
locators, required lanes, inherited tags, Gherkin, and base/head changes. It
checks bookkeeping, not semantic truth.

Review each mapping with one question: **could the named defect remain while
this evidence stays green?**
