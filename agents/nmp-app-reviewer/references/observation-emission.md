# Defect class: undiffed wholesale snapshot emission

An app that embeds NMP usually grows a **second** observation transport of its own —
its app-owned Rust core pushing app-specific state (sessions, workspaces, catalog,
receipts, diagnostics) across its own FFI boundary to Swift or Kotlin. NMP's own
incremental emission machinery is internal to NMP's facade and governs NMP's data; it
does not extend to the app's state. So the app builds its own transport, and that is
the point at which it usually drops the discipline NMP applies to its own.

The defect: **every tick pushes the full current state, unconditionally, to every
observer, regardless of what changed.**

## Why this is a real finding, not a micro-optimization

Incremental emission is a measured NMP lesson, not a preference. `docs/design-record.md`
lists ADR-0070 incremental emission as a hard-won operational result with a measured
18% effect, alongside relay transport specifics — the class of lesson the record
explicitly warns a rewrite would regress.

The steady-state shape it encodes: emit a typed projection's value **only when that
projection actually changed**, and treat a full snapshot as the cold-start and resync
baseline rather than the per-tick default.

An app that resends everything every tick has reinvented the pre-ADR-0070 behavior on
its own side of the wire.

## The three symptoms, in order of severity

**1. Unscoped payload.** One snapshot type bundles unrelated concerns — all sessions,
all bindings, all pending writes, all receipts, all workspaces, recent activity, recent
errors, boundary refusals — instead of being shaped to what a screen has open. Cost then
scales with total app state rather than with active views.

**2. No diff.** The frame is rebuilt from scratch and delivered in full on every wakeup.
Nothing compares this tick's projection against the last delivered one, so a change to
one field costs the encoding and crossing of every field.

**3. One shared trigger.** A single revision counter (`watch::Sender<u64>` or
equivalent) is bumped from many unrelated call sites — install, grant, session
open/close, workspace CRUD, receipt, pending write, error — and any one of them wakes
the loop and resends everything. Grep the bump helper: a double-digit bump-site count
against one payload is the tell.

### The coalescing trap

`watch` coalescing is not a defense, and reviewers accept it as one too easily. It
collapses rapid-fire bumps between ticks into a single wakeup; it does not make any
single tick's payload smaller. Burst coalescing and payload diffing solve different
problems. An app that answers "we use `watch`, so it's coalesced" has answered a
question you did not ask.

## Severity calibration

Rank by scope and tick frequency, not by how the code reads:

- **Blocking** — whole-app snapshot, unscoped to open views, undiffed, driven by a
  shared counter bumped from unrelated mutations. Wrong on all three symptoms.
- **High** — a purpose-scoped, well-bounded projection that still full-resends per tick
  from a high-frequency source. Per-relay, per-subscription, and per-kind diagnostic
  counters tick often, so an otherwise exemplary dedicated type can be the most
  performance-sensitive instance of the pattern in the codebase, not the least.

Say the second case out loud in the review. A team that built the scoped, bounded,
lazily-opened version did the harder work already, and a finding that reads as if they
did nothing gets dismissed along with the part that was right.

## Credit what was done right

Marks of an app that understood the boundary even where it missed the diffing. Name each
one explicitly when present:

- a dedicated, purpose-scoped snapshot type rather than another field on the god frame;
- opened lazily and held open only while something observes it, so an unwatched feature
  costs no relay accounting;
- a bounded observer set with an explicit capacity;
- explicit `omitted_*` counts on every collection instead of silent truncation — the
  "report shortfalls, never claim completeness" guardrail applied correctly.

The type split is usually not the problem. **Delivery is.**

## The fix shape

1. Keep or complete the split into typed per-screen, per-concern projections.
2. Retain the last delivered value per projection and emit a change row carrying that
   projection's full current value only when its encoded bytes differ.
3. Bound and coalesce emit rate **per projection**, instead of letting one shared
   revision counter drive every projection's cadence.
4. Reserve full-snapshot delivery for cold start and explicit resync.

## How to audit it

- Find every `observer.update(` or equivalent delivery call and read what constructs its
  argument. Unconditional construction inside the loop body is the defect.
- Read the snapshot type's field list. Ask which single screen needs all of it at once.
  If none does, it is unscoped.
- Count the trigger's bump sites and check how many are unrelated to the payload.
- Ask what the app compares against the previous delivery. If the answer is nothing,
  there is no diff, whatever the transport is named.
- Establish the tick source's real frequency. A slow source lowers severity; it does not
  make the pattern correct.

## Reporting note

The app may name this rule under its own doctrine identifiers. Cite that document only
if you have opened it in the checkout in front of you. Where the app has no such
doctrine, argue from ADR-0070 and the measured result in `docs/design-record.md`, and do
not attribute to NMP a rule you have not read.
