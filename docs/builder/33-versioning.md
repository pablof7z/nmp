# Building against a governed provisional API

NMP has not promised v2 source compatibility. Public names and record layouts
may change when a falsifier proves the current shape unsafe or incomplete.

"Provisional" does not mean casual drift. Public changes require evidence,
cross-surface/persistence impact review, explicit human signoff, synchronized
Rust/Swift/Kotlin falsifiers, and removal of the superseded path.

## What builders can rely on

The current architecture frame is:

- NMP is an embeddable engine, not an app framework.
- Live query and write intent are the two workload nouns.
- Engine decisions consume closed, introspectable values.
- Core has no preferred content kind or feed policy.
- Query snapshots report scoped evidence, not global Nostr truth.
- Durable acceptance owns one canonical pending row plus the obligation.
- Every write has observable, reattachable receipt facts; durability controls
  whether the publication obligation resumes.
- Signer selection has a current-pubkey default, per-write override, and is
  pinned at acceptance.
- Protocol modules own exact schemas and compose immutable drafts/context.
- One engine is one shared cache trust domain with explicit destructive reset.
- Limits produce exact chunking, rejection, backpressure, or shortfall, never
  silent first-N substitution.

New evidence can still revise one of these, but not as an incidental API patch.

## What is expected to move

Known pressure exists around:

- filter-only query identity becoming selection + source + access;
- aggregate coverage becoming cache/acquisition/shortfall evidence;
- current account splitting into current-pubkey input and signer providers;
- crash-safe receipt/pending-row records;
- optional protocol-module packaging; and
- Android/native secure-provider projection.

The guide uses one coherent illustrative spelling so the product can be
evaluated. It does not promise those identifiers.

## Upgrade discipline before v2

1. Pin a commit or release tag.
2. Read the surface-change entry and [current status](03-status-map.md) before
   upgrading.
3. Regenerate/rebuild matched Rust and native bindings together.
4. Run the platform and protocol-module falsifiers your app depends on.
5. Delete use of the superseded shape rather than keeping compatibility
   adapters in application code.

Do not infer stability from one example compiling. The supported facade and its
governance snapshots define the actual surface.

## Compatibility starts at v2

Before declaring v2, NMP must prove one canonical facade, crash-safe writes,
scoped evidence, bounded delivery, signer providers, protocol composition,
permanent diagnostics, and Rust/Swift/Kotlin parity. Only then should semantic
versioning turn current provisional shapes into compatibility promises.

---

<sub>[Index](README.md) · Related: [Current status](03-status-map.md) · [Guarantees](28-patterns.md) · [Protocol module authoring](32-extending.md)</sub>
