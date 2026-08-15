# Architecture boundaries

Where a decision ends, where a commit begins, and what is allowed to happen
before that commit. This is the map `AGENTS.md`'s fifth standing convention
assumes you have.

It is not a style guide and not a review step. Nothing here is a question you
answer in a PR description.

## The mental model, in one paragraph

Intent arrives as an immutable, inspectable **value**. A **deterministic
decision** turns explicit current facts plus that value into a next semantic
state and a set of actions. Where persistence is authoritative, that decision
runs inside **one transaction** that reads a consistent snapshot, applies its
writes, and commits — and the commit is the truth boundary. What comes back
out are **committed facts**. External work is emitted as **typed effects**
*after* the state authorizing it is durable, and the **runtime interprets**
those effects and returns their completions as facts correlated to the exact
operation and generation that caused them. Changed inputs and committed facts
then propagate to the **precisely affected** dependencies.

Each of those five bold terms is a real boundary in this codebase, not a
target. The rest of this document says what they mean here, and — where the
code does not yet meet the model — says so.

## What "functional" means in NMP

It means a decision can be understood and falsified without hidden I/O,
clocks, randomness, callbacks, or unrelated mutable global state.

It does **not** mean immutable-everything, persistent whole-engine values, an
effect monad, or a framework. Rust ownership plus one clear mutation owner is
usually simpler and safer than any of those. Local mutation inside a clearly
owned aggregate is fine.

The load-bearing consequence is testability, and it is the reason to care:
`EngineCore` is driven headlessly by scripted messages with no sockets and no
real relays, which is what makes ledger falsifiers cheap enough to actually
write. That property is worth protecting. A general effect DSL would relocate
the logic into an interpreter and buy nothing.

**Do not call something pure because it does no socket I/O.** `EngineCore`
performs durable store reads and writes; it is a *deterministic, headless
semantic coordinator*, and the older "PURE synchronous reducer" phrasing in the
dated milestone plans is wrong in a way that teaches the wrong distinction. The
useful property is determinism given explicit inputs, not absence of effects.

## What "reactive" means in NMP

It means dependency-driven demand and projection updates:

```text
changed input or committed store fact
    -> precisely affected dependency nodes
    -> changed concrete demand / projections
    -> bounded observation updates
```

It does **not** mean Rx, and NMP is not FRP. Do not turn every operation into a
stream.

One distinction matters more than the rest, because conflating it loses data:

- **State** may conflate to the latest semantically correct value. Query rows
  and diagnostics work this way — a slow consumer sees the current answer, not
  every intermediate one.
- **Causality** may not be silently dropped. Durable receipt facts,
  cancellations, signer completions and lifecycle terminals are each an event
  that happened; conflating them loses the fact that it happened at all.

**Current honest exception.** Reactivity is dependency-precise in the resolver's
demand graph, which recomputes only from reactive nodes and propagates to a
parent only when a value actually changed. It is *not* precise in the
row-projection layer: several paths reopen the store for every live observation
and rely on a downstream diff to suppress the resulting no-op effects. No wrong
data reaches an app, but the work is real and scales with the number of open
observations. **#1646** owns this. Do not cite NMP as uniformly precise until it
closes.

## Transaction and effect rules

1. **Read, decide and write atomically where correctness requires it.** The
   commit is the truth boundary. Do not mutate authoritative in-memory state or
   report success before it.
2. **No irreversible external I/O inside a transaction.** No signer call, no
   relay send, no network round-trip between `begin_write` and `commit`.
3. **Persist the obligation before the effect that discharges it.** If external
   work must survive a crash, its record commits first and recovery resumes it.
4. **Effects are values, not calls.** A decision returns typed effects; the
   runtime performs them afterward. This is what makes the ordering in rules 1–3
   checkable by reading one function instead of tracing a call graph.
5. **Completions return as correlated facts.** A completion carries the
   generation or token of the request that caused it, and a mismatch is dropped
   rather than applied.

Rules 1, 2 and 4 hold today on the write-acceptance and cancellation paths, and
bug-class-ledger **#9** is the falsifier that keeps them honest: acceptance is
constructible only after intent, receipt, frozen body and canonical row commit
together. Rule 5 holds for writes (a generation counter) and for AUTH (epoch,
phase-token and capability-instance all checked).

Do **not** build a general store-effect DSL to make reads and writes look pure.
Keep transaction orchestration direct and narrow; extract the calculation
*inside* it when that improves reasoning or testing.

## Ownership rules

- **One owner per lifecycle, retry, queue, resource and connection.** Two owners
  of one property is the defect the Noun Gate exists to catch.
- **The runtime executes; the semantic owner decides.** The runtime owns
  threads, sessions, cancellation, scheduling and delivery. It does not acquire
  policy merely because data passes through it.
- **The store commits durable truth.** Redb is the one production store and it
  is concrete — there is no backend abstraction, and reintroducing one requires
  a real second backend, not the possibility of one (see #1495).
- **The app owns product policy** and which intent exists at all.

Keep the four vocabularies distinct at least locally, in types or modules:
**commands** (change or acquire something), **facts** (something already
happened), **effects** (work a decision authorized), and **committed facts**
(durable mutations that crossed the commit boundary). Do not promote that
distinction into a framework of tiny traits — introduce a type when it excludes
a real bad path or makes a real ownership boundary testable, and not otherwise.

**Current honest exceptions.** The engine command inbox is unbounded (**#786**),
and AUTH state is entirely in-process with no durable backing — a restart loses
it. Both are known and tracked; neither is a licence to add a third.

## How to use this document

Read it once. When you touch a module, leave its decision boundary, ownership,
effects and tests clearer than you found them — that is the whole obligation.

The examples above deliberately cite **bug-class-ledger entries and their
falsifiers** rather than symbol names, because symbols move: `EventStore` and
four transport surfaces were deleted in the same week this was written. A ledger
entry names an excluded bug class and the mechanism that excludes it, and stays
true across the refactor that renames its implementation.

If something here contradicts the code, the code wins and this document is a
bug. Fix it in the same change.
