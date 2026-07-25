# Offline-first and optimistic publishing

Offline-first is the default an NMP app should be guided toward, not a feature it
bolts on later. NMP already owns the durable write obligation, the retry schedule,
and the per-relay outcome. The app's job is to accept the user's action, show
honest delivery state, and survive process loss — and most apps get this wrong in
the same two directions: they invent an optimism NMP did not give them, or they
throw away the durability NMP did.

Guide apps toward this shape proactively. Do not wait for a bug report; the
failure mode here is a post that silently never went out, which nobody files a bug
about because the UI said it was fine.

## The shape that is correct

1. Restore and activate the intended signer/account first.
2. Construct the `WriteIntent` with deliberate durability and routing.
3. Publish, and persist `receipt.id` in app-owned durable state **immediately**.
4. Observe receipt facts **independently** from the query that renders the
   canonical row.
5. On restart, reopen the same store, restore the same identity, and reattach the
   receipt by id.
6. Distinguish attached, not found, and retained-but-unreadable.
7. Drop the app's receipt pointer only under an explicit retention policy, after
   terminal evidence has been handled.

## Optimistic publishing: what the app may and may not do

**The row is not an optimistic overlay created from the draft.** An app that
renders its own composed draft into the feed has built the pending-row mirror the
guardrails forbid, and it will diverge from canonical storage the moment the
signed event differs from what the app assumed.

**Delivery UI is receipt-centric, not row-centric.** Before `Signed(eventId)`, the
public row exposes no intent or receipt id. There is nothing to correlate a feed
row against yet, so the "sending…" affordance must be driven by the receipt.
Correlate to a feed row only once the signed event id exists.

That is the honest version of optimism: the user sees their action acknowledged
immediately, driven by receipt facts, while the canonical row arrives through the
query stream when it is real. The app never fabricates the row.

## Retry lane facts are evidence, not commands

An offline-first UI lives or dies on reading these correctly:

- `AwaitingRelay` — a lane waiting for connectivity. Offline time does not consume
  an attempt. This is the normal, healthy offline state; an app that renders it as
  an error trains users to panic.
- `AwaitingAuth` — an authentication pause, with no polling deadline.
- `RetryEligible` — the engine-owned durable scheduler's persisted attempt ordinal
  and eligibility time. It is **not** an app retry verb. An app that renders a
  "Retry" button wired to its own republish has invented a capability and risks a
  duplicate obligation.
- `HandoffAmbiguous` — preserves the exact attempt and observation time without
  claiming a send. It must survive to the UI as ambiguity.
- `Sent` — only where the exact durable lane has a persisted `Written` handoff.
  Queue acceptance, pre-wire attempt start, ambiguity, and ephemeral transport work
  are **not** sent facts. An app that shows a checkmark on queue acceptance is
  lying to the user.

## Restart honesty

Reattachment reconstructs persisted relay and AUTH waits, retry eligibility,
ambiguous handoffs, and `Sent` only where an exact durable lane persisted
`Written`. It does not reproduce transient routed history or invent ephemeral
handoffs. An app needing a complete historical activity log must journal live
progress separately, and must not present the reattached view as that log.

**The lost-id window is real and must be stated, not papered over.** Receipt
enumeration does not exist, so process loss after a successful publish return but
before the app persisted the id leaves an obligation the app can no longer name.
Apps must state that limitation rather than claim perfect recovery, and must not
blindly publish a replacement for an obligation whose id is unknown — that turns
one uncertain delivery into two certain ones.

This is why step 3 says *immediately*. Any work between the publish return and the
id write widens the window.

## Observed defects and their tells

### Assuming one payload shape destroys an approved signature

An adapter parsed every approved draft as an unsigned event and built an unsigned
write payload unconditionally. An already-signed event — approved by the user as
exact bytes — lost its signature and was re-signed on the way out. The author
check ran only on the unsigned branch, so the branch that mattered had no check at
all.

- **Tell:** an unconditional parse into the unsigned type on a path where a signed
  event can legitimately arrive; a single `WritePayload::Unsigned(...)`
  construction with no signed branch.
- **Consequence:** what the user approved is not what gets published. It also
  collapses the governed sign-only distinction, where the whole point is that the
  exact returned event is verified.
- **Fix:** attempt the signed parse first and preserve what was approved; run the
  frozen-account author check on **both** branches, not just the fallback.

### A revision counter used as a change signal

A receipt model published a monotonic `revision` from the projection, and the view
scheduled a durable layout save on every revision change. The thing the save
actually depended on was the set of retained receipt ids. Every unrelated bump
wrote durable state again.

- **Tell:** any `revision`, `generation`, or sequence counter driving a durable
  write, a persistence decision, or a diff. Look for change-observers keyed on a
  counter whose body reads a different value.
- **Consequence:** durable writes proportional to tick rate rather than to real
  change — which on an offline-first path is exactly when ticks are most frequent.
- **Fix:** publish and observe the value the decision depends on.
- **Same false belief as** `observation-emission.md`: that a monotonic counter is
  a proxy for "the thing I care about changed." Producers push everything on every
  bump; consumers act on every bump. Cite both together when you see either — an
  app exhibiting one usually exhibits the other.

## Audit questions

- Where does the pending affordance get its state — the receipt, or the app's own
  draft?
- Is `receipt.id` persisted before anything else happens after the publish return?
- Does any UI element claim delivery on something short of `Sent` with persisted
  `Written`?
- Does `AwaitingRelay` render as a normal offline state or as a failure?
- Is there a user-facing retry that republishes? Who owns the duplicate risk?
- Does the app state the lost-id window, or claim perfect restart recovery?
- Does the write path assume one payload shape?
- Does anything durable key off a revision counter?
