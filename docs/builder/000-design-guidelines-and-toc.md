# Builder guide design rules

This file governs the published builder guide. It is an editorial contract,
not another architecture authority. `README.md`, `docs/VISION.md`, the focused
contracts under `docs/design/`, and the open GitHub issues own product truth.

## 1. Write the coherent target

Conceptual chapters describe the provisional v2 developer experience as one
system. They do not alternate sentence by sentence between the current API and
the intended API.

One banner on the guide landing page states that public names are illustrative.
The [current implementation status](03-status-map.md) and
[`docs/known-gaps.md`](../known-gaps.md) state what ships. A chapter may call out
a current incompatibility when a developer could otherwise damage data or ship
an incorrect claim, but implementation history is not the narrative spine.

## 2. Preserve the two nouns

Every app workload is one of:

- a live query observed as the platform's native reactive primitive; or
- a write intent observed through receipt facts.

Identity inputs, capabilities, diagnostics, and protocol modules configure or
explain those nouns. They do not become an NMP session, resource hierarchy,
container, provider, app reducer, or lifecycle framework.

## 3. Keep core content-neutral

No quickstart, helper catalog, or chapter structure may imply that kind:1, a
timeline, follows, profiles, or any other content family is the center of NMP.

- Core examples use caller-selected kinds or a clearly app-owned example kind.
- Protocol examples name their owner, such as NIP-02, NIP-29, or NIP-68.
- A reusable helper expands to the closed public grammar and is never a hidden
  subscription lifecycle.
- A protocol module owns only the exact schemas and semantics its protocol
  defines.

Use multiple kinds and protocol shapes across the guide. Diversity is evidence
that the core abstraction is genuinely generic.

## 4. Show values in, code after

Anything that changes engine demand, source authority, access, routing,
admission, signing identity, or persistence crosses the boundary as a closed,
typed, printable value. Do not show app closures in those positions.

Arbitrary code is welcome after delivery: fold snapshots, rank rows, format
content, choose labels, navigate, and render in the app's own architecture.

## 5. Never imply global completeness

Use the snapshot vocabulary consistently:

- canonical rows from the local cache;
- cache revision/provenance evidence;
- acquisition facts for currently planned sources and access contexts; and
- explicit shortfall when a source or local limit prevented intended work.

Do not write `syncHealth`, `globally synced`, `authoritative empty`, or any
equivalent promise. EOSE and watermarks are scoped source facts.

## 6. Make writes locally reactive through the store

Durable `Accepted` means one crash-atomic persistence boundary owns the frozen
body, signer identity, receipt, obligation, and canonical pending row. Matching
queries see that row through ordinary store invalidation. The write path never
pushes an optimistic row directly to an observer.

Every durability class has observable receipt status. A non-durable write may
forgo persistence and restart reattachment, but it is not a silent `void`
operation.

## 7. Separate current pubkey from signer identity

`$currentPubkey` is a reactive query input and ergonomic default signer. A
write may override the signer identity without changing current pubkey. Once an
intent is accepted, the chosen identity is pinned.

The app owns its account list and identity policy. One engine has one shared
cache trust domain; switching accounts does not partition stored rows.

## 8. Treat protocol modules as semantic owners

An opt-in module may provide builders, parsers, state reconstruction, derived
query fragments, semantic operations, and typed source/routing context for the
exact protocol it owns. It may not add a second store, engine, signer path,
subscription manager, or app framework.

Composition is immutable. A NIP-29 group operation may add an `h` tag and host
relay context to a NIP-68 photo draft without claiming ownership of the photo
kind. Core freezes and signs the final value once.

## 9. Use native platform idioms

The semantic values agree across Rust, Swift, and Kotlin. Observation and
ownership follow each platform:

- Rust: one facade plus `Stream`/receiver and `Drop`;
- Swift: `AsyncSequence`, optional `@Observable` convenience, and ARC;
- Kotlin: cold `Flow` and coroutine cancellation.

Do not promise a platform merely because the values are serializable.

## 10. Keep examples honest without freezing spelling

Every target code block is illustrative. Prefer small, internally consistent
spellings over hedging each line. State uncertainty once near the example.

When an implementation frame changes a public concept, update the guide in the
same PR only after the governed change record explains the evidence,
cross-surface impact, persistence impact, and superseded path.

## 11. Navigation and size

- Every chapter has one H1 and a short purpose statement.
- Use relative links and a previous/index/next footer.
- Keep every documentation file at or below 800 lines.
- Prefer one canonical explanation and link to it from adjacent chapters.
- Keep the shortest path from landing page to first query and first write under
  seven chapters.
