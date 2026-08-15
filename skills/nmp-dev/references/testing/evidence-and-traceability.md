# Evidence and traceability

Every `built` scenario needs executable evidence that proves its promise.
There is no CI: the required checks must be runnable on demand and must
distinguish situations that should behave differently. Deliberately
reintroducing the named bug must make them fail. Nearby green checks do not
count.

## Locators

A locator is a metadata line that names a test, validation script, or live
integration run. Use one for each check the scenario depends on:

```text
# nmp:evidence=<kind>:<owner>::<target>
```

Kinds: `rust`, `swift`, `kotlin`, `parity`, `script`, `live`.

Examples:

```text
# nmp:evidence=rust:nmp-router::literal_demand_remains_pinned
# nmp:evidence=swift:NmpTests::receipt_reconstructs_after_restart
```

Use stable check names, not line numbers. Each non-live locator must identify
exactly one enabled test or validation script that actually runs. An ignored,
conditionally compiled, manual-only, masked, ambiguous, symlink-backed, or
similarly named check does not qualify. Nothing runs these checks
automatically, so a locator is honest only if a reviewer actually ran the
named check before marking the scenario `built`. A `live` locator names a run
against a real service. It may add field evidence but cannot replace a
repeatable check.

List every layer needed to prove the public promise. One property or model test
may support several scenarios when it genuinely distinguishes them. Do not
duplicate tests only to create one locator per scenario.

## Deliberate-break checks

A falsifier is a small deliberate break that the linked evidence must catch. Every
`built` scenario describes one with `nmp:falsifier`:

```text
# nmp:falsifier=Treat one relay's EOSE as global completion; this scenario fails.
```

For new or changed evidence:

1. Remove, bypass, or reverse the protection named by `nmp:falsifier`.
2. Confirm the linked evidence fails because of that break.
3. Restore the protection and confirm the evidence passes.

If the evidence still passes, it does not prove this promise.

## Prove external effects independently

When practical, prove an external effect with evidence outside the code being
tested:

- Use relay or signer logs to prove external effects.
- Close and reopen through the public API to prove durability.
- Use the typed frames and receipts returned to an app.
- Compare bytes across Rust and native platforms for platform claims.

Diagnostics explain behavior but should not self-certify an external effect
when an independent witness is practical.

## Status

Mark a scenario `built` only after the behavior exists, every locator passes,
the deliberate-break check works, and all restart, platform, and failure cases
required by the promise pass.

Use `specified` when the agreed behavior still has an `implementation`,
`evidence`, `fixture`, or `platform` gap. Use `known-violation` when current code
contradicts the intended behavior. Both require a readable open issue. A
missing or closed issue makes the metadata invalid.

## Acceptance

An `@acceptance` scenario must:

- be `built` and link a `rust:nmp::<test>` test;
- perform and observe the behavior through the public Rust `nmp` API or an
  external witness;
- set up inputs without performing the behavior under test;
- run the exact build under review; and
- prove that it cleans up its processes and resources.

An acceptance test may call internal components only to set up inputs, and each
such call needs a specific reason. The transitional `nmp-bdd` runner does not
prove behavior through the public API.

A detached checker crate used to check metadata, unique IDs, open
issues, locators, required CI jobs, inherited tags, Gherkin, and changes from
base to head. That crate and the CI that ran it are deleted along with the
rest of the CI-era tooling, so nothing now mechanically verifies that these
records are connected. Even when it existed, it could not decide whether the
tests prove the stated behavior — that judgment call has always belonged to
the reviewer, and it is now the only check left.

For each evidence locator, ask: **could the named bug still exist while this
evidence passes?**
