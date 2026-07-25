---
slug: review-nmp-app
summary: Use when asked to review, audit, or form an opinion on an application built on the NMP public facade.
triggers:
  - Review this app.
  - Does this use NMP correctly?
  - Audit our NMP integration.
  - What's wrong with how we're consuming NMP?
status: draft
created: 2026-07-25
updated: 2026-07-25
---

## Outcome

A ranked findings list the app's owners can act on without rereading their own
codebase: each finding carries a one-sentence claim, evidence at `path:line`, the
consequence a user or operator would see, and the fix shape. Plus an updated
ownership map in notes for the next review.

## Inputs

- The app checkout, and which facade tier it consumes (direct Rust, Swift, Kotlin,
  or the optional content/UI packages).
- The NMP checkout the app builds against, for verifying every named symbol.
- Prior notes for this app, if any.

## Sources of truth

- The `nmp` skill for the supported surface; its source map for exact symbols.
- The NMP checkout itself, when the skill's verified revision differs from it.
- `references/observation-emission.md` for the app's own FFI transport.
- Never `docs/VISION.md` or roadmap material as proof a method exists today.

## Approach

1. Read prior notes first. Reverify anything they name before relying on it.
2. Record the app's facade tier and the NMP revision reviewed. Check the skill's
   verified revision against the checkout; run its validator on drift and carry the
   result as a caveat on the whole review.
3. Build the ownership map before reading feature code — what the app delegated to
   NMP, what it kept. Most real findings are visible at this level alone. Look for
   the boundary errors first: app-side websocket subscriptions or relay routing, a
   second durable event cache treated as truth, an `isSynced` boolean derived from
   EOSE or one relay, an observation created per render, publish acceptance treated
   as delivered, typed refusal collapsed into a timeout or an empty stream, an
   internal crate or raw generated binding imported to dodge an ergonomic gap.
4. Walk the surface in order: query construction and source authority → row
   accumulation and presentation state → write intents and receipt consumption →
   identity and signer handling → lifecycle and teardown → the app's own FFI
   observation transport → diagnostics and what the UI claims.
5. Verify each suspected defect against the actual facade. Discard what does not
   survive verification rather than hedging it into the report.
6. Rank: structural (wrong ownership, wrong data) → correctness (right ownership,
   wrong handling) → drift (works now, breaks on a supported change) → taste.
7. Update notes: ownership map, accepted deviations and the owner's stated reason,
   recurring patterns, unresolved questions.

## Judgment gates

- Exposed secret keys in logs, fixtures, screenshots, or source: stop and report
  immediately, out of band from the ranked list.
- A defect whose honest fix is an NMP-side change rather than an app-side one:
  name it as such and ask before writing an app-side workaround into the report.
- A finding that contradicts a deviation the owner already accepted: only raise it
  if the cost has changed, and say what changed.

## Output

Ranked findings first. Advisory taste items last and labeled as taste — ordering,
moderation, navigation, formatting, account UX, and feature policy are app-owned.
State plainly what was not reviewed. No praise padding.

## Done when

Every finding is verified and ranked, unreviewed areas are named, and notes are
updated.

## Failure and recovery

- Cannot verify a symbol against any checkout: report the claim as unverified
  rather than asserting it, and say what checkout would settle it.
- App is too large to review whole: scope by user-visible feature, review those
  fully, and name the rest as unreviewed. Never report partial coverage as
  complete.
