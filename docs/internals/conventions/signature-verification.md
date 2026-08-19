---
title: A signature is verified once, on the way in, and never again
category: conventions
slug: signature-verification
status: policy
date: 2026-08-17
owns:
  - the rule that schnorr verification happens exactly once per event, at ingest
  - every sanctioned door an event can enter the store through, and what each proves
  - what "events in the store are known-good" does and does not cover
  - the rule that nothing on the read path re-verifies
  - the signature-comparison rule for a redelivered event, and why a mismatch is not misbehaviour
related:
  - docs/internals/conventions/schema-epoch-discard.md
  - docs/internals/conventions/no-backwards-compatibility.md
issues:
  - "#1782"
  - "#1800"
---

# A signature is verified once, on the way in, and never again

**Owner ruling, 2026-08-17.** Verification is a cost paid at the boundary. Once an
event is in the store its signature is settled, and no later code path may spend
schnorr on it again.

## The four rules

1. **An event is schnorr-verified exactly once**, at the moment it is admitted to
   the store, at whichever door admitted it.
2. **Events in the store are known to carry a correct signature.** Downstream code
   may rely on this without checking.
3. **A redelivered event is settled by comparing signature bytes**, never by
   schnorr. Equal means the relay sent a good event. Not equal means skip it.
4. **Nothing on the way out of the store verifies.** Not at boot, not on read, not
   on recovery, not in a codec.

Rule 4 is the one that gets violated, because `verify()` is always in scope and
always looks harmless. It is not: it is a schnorr operation on the engine thread,
scaling with whatever the caller is iterating.

## Rule 1 is about admission, not about relays

An event reaching the store from a relay is the common door, not the only one.
There are six, and three do not involve a relay at all. The rule is that **every
door proves the signature before admitting**, by one of the sanctioned means below.

| Door | What proves it | Means |
|---|---|---|
| Relay `EVENT` frame → `insert` | id recompute (`nmp-transport/src/pool/inner.rs`) plus schnorr in the verifier pool (`pool/verify.rs`) | schnorr |
| Relay redelivery of a known id | byte comparison against the stored signature (`pool/verify.rs`) | comparison — see rule 3 |
| Local publish, NMP signs | `VerifiedSignature::verify` → `promote_signed` | **type proof** |
| App-supplied signed event across FFI | schnorr at accept and again at promote | schnorr |
| Semantic / replaceable materialization | sentinel guard at promote | invariant |
| `insert`'s adoption path | **nothing — a comment (#1800)** | *unsound* |

Two things follow that the four-rule summary does not say on its own.

**The id recompute is load-bearing, not a pre-filter.** The gate does not call
`Event::verify()`; it recomputes the id in one module and checks the signature in
another. The schnorr half *alone* proves nothing, because it signs off on whatever
id the relay claimed. Both halves are required and they must not drift apart.

**`VerifiedSignature` is the only real type proof, and it is sound.** Its fields
are private, `verify` is its only constructor, and there is no struct-literal
construction anywhere in the workspace including tests. It is what a capability
token should look like. It does not, however, survive into the store — `insert`
takes a bare event, and the guarantee downgrades to control flow at that point.

## Rule 2 excludes pending rows

"Known good" means **rows carrying a real signature**. A row may sit in the store
with the *sentinel* signature, which nobody has ever verified, while a local write
waits to be signed. Those rows are guarded at the read door and at the semantic
promote door, and are **not** guarded on `insert`'s adoption path. That gap is
#1800.

"Known good" also means *currently stored*, not *ever verified*. A refused,
expired, superseded, GC'd or tombstoned event leaves the id index, so a later
redelivery finds no stored signature and takes the schnorr path again. That is
correct, and it means "once" is once per residency, not once per lifetime.

## Rule 3: a mismatch is not evidence of a lying relay

**One event id has arbitrarily many valid signatures.** NIP-01's id preimage is
`[0, pubkey, created_at, kind, tags, content]` — the signature is not covered by
the id — and `nostr` signs with `OsRng` auxiliary randomness, so signing the same
event twice yields different bytes and both verify. Verified by execution.

So NMP pins the first signature it stored, and that is a **policy**, not a
detection mechanism:

- **Equal** — the relay sent a good event. Accept it, no schnorr.
- **Not equal** — skip it.

A relay holding a different valid signature is honest. Skipping is the cheap,
correct-enough behaviour; concluding misbehaviour from it is not sound, and no
code may treat a mismatch as evidence of a hostile relay.

Note the consequence honestly: a skipped event is dropped before it becomes a
frame, so a live query can silently lose it.

## Rule 4: what deleting the read-path checks costs

Removing the read-path verification removes the **only** integrity check those rows
have, and this is an accepted trade rather than an oversight:

- redb verifies page checksums on the repair path after an unclean shutdown, never
  on ordinary reads, and NMP never calls `check_integrity`.
- NMP's row codec validates a magic, a version byte, reserved bytes and section
  bounds. **Nothing covers the payload.** A bit flip inside `content`, or inside
  the 64 signature bytes, is undetectable at every layer.

If integrity detection is ever wanted, the instrument is a checksum in the row
codec — cheap, and it covers the whole payload. It is **not** schnorr, which costs
milliseconds and only covers the signed fields. Re-verification is not an integrity
strategy and must not be reintroduced as one.

## Failed verification is not remembered

An event that fails schnorr is dropped and nothing durable records it. A relay
redelivering the same forged event forces a fresh check every time. The only trace
is an in-memory counter, scoped to the connection generation and lost on reconnect
and restart.

This is a known property, not a rule. If it ever matters, the answer is a negative
cache, not re-verification.

## Signer output is a different act

Two sites verify what a signer just produced — NIP-42 AUTH and sign-only
operations. That is checking a signer's output before use, not re-verifying
stored data, and it mints no `VerifiedSignature`. These are outside this convention
and must not be deleted in its name.

## Instrumentation must not be able to lie

Under `bench-instrumentation`, both halves of the ingest gate can be switched off at
runtime — and the schnorr-call counter still increments. A falsifier that reads
normal while nothing is being checked is worse than no falsifier. Any counter
standing in for "verification happened" must be incremented by the verification
itself, not beside it.

## What this convention forbids

- Calling `Event::verify()`, or any schnorr operation, on data read from the store.
- Treating a signature mismatch on redelivery as misbehaviour.
- Introducing a schnorr check as a corruption or integrity check.
- Admitting an event through a door that proves nothing (the standing violation is
  #1800, and it is a violation, not a precedent).
- Splitting the ingest gate further, or letting the id recompute and the signature
  check drift apart.
