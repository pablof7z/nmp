---
slug: audit-observation-emission
summary: Use when auditing how an NMP-consuming app pushes its own state across its own FFI boundary, or when a reviewer suspects full-snapshot-per-tick emission.
triggers:
  - Is our FFI transport doing too much work?
  - Why is the app slow when nothing changed?
  - Review our runtime snapshot / observation frame.
  - Check the Rust to Swift update path.
status: draft
created: 2026-07-25
updated: 2026-07-25
---

## Outcome

A verdict per observation channel: scoped or not, diffed or not, triggered by what,
at what real frequency — with severity ranked and the fix shape named.

## Inputs

The app's own Rust core and its FFI crate. This audit is about the **app's**
transport for **app-specific** state, not about NMP's internal emission.

## Sources of truth

- `references/observation-emission.md` — the full defect class, the coalescing trap,
  severity calibration, and what to credit.
- `docs/design-record.md` in the NMP checkout for the ADR-0070 anchor.
- The app's own architecture doctrine only if you have opened it in this checkout.

## Approach

1. Enumerate the channels. Grep for observer registration and every
   `observer.update(` or equivalent delivery call.
2. For each channel, answer four questions in this order:
   - **Scope** — read the payload type's field list. Which single screen needs all
     of it at once?
   - **Diff** — what does the code compare against the last delivered value? If
     nothing, there is no diff.
   - **Trigger** — grep the bump helper. How many call sites, and how many are
     unrelated to this payload?
   - **Frequency** — what actually drives the tick, and how fast can it go?
3. Reject the coalescing defense explicitly if it appears: `watch` collapses bursts
   into one wakeup, it does not shrink a tick's payload.
4. Rank: unscoped + undiffed + shared trigger is blocking; scoped and bounded but
   still full-resending from a fast source is high.
5. Name what the app got right before naming what it got wrong — dedicated scoped
   types, lazy open while observed, bounded observer capacity, explicit `omitted_*`
   counts.

## Output

Per channel: `path:line`, the four answers, severity, and the fix shape — split into
typed per-screen projections, emit only when encoded bytes differ, bound and coalesce
per projection rather than off one shared counter, full snapshot for cold start and
resync only.

## Done when

Every delivery call site has a verdict, and no channel is left described only as
"probably fine".

## Failure and recovery

If the tick source's frequency cannot be established from the code, say so and rank
on scope and diffing alone rather than guessing a rate.
