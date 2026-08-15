# AGENTS.md

Canonical contributor guide for the NMP repo. Every rule here applies to agents and humans alike. Keep it concise and plain: durable understanding lives in `docs/`, and temporary work lives in GitHub Issues. Execution plans are temporary work artifacts, not architecture authority; move lasting decisions into the document that owns the subject and let git preserve the plan's history.

## Cold-start reading order

1. `README.md` — what NMP is (two nouns: a live query, a write intent) and the honest current status.
2. `docs/VISION.md` — the north star, the milestone plan (M0–M6), the two thesis-gates, and the numbered principles (P1…) work is measured against.
3. `docs/design-record.md` — why the architecture is shaped this way (the first-principles exploration and the decisions that fell out).
4. `docs/bug-class-ledger.md` — the bug classes structurally ruled out, and the mechanism that rules each out. This replaces governance-by-policing: correctness lives in the shape of the API, not a police force patrolling it.
5. `docs/known-gaps.md` — the truth-anchor companion: everything built-but-incomplete or deliberately deferred, so nothing hides.
6. `docs/internals/architecture-boundaries.md` — where a decision ends, where a commit begins, and what may happen before it. What "functional" and "reactive" mean *here*, the transaction/effect rules, and the ownership rules — plus the current honest exceptions to each.
7. **GitHub Issues** — the single tactical tracker: what is being worked on, what is queued, and the *why* behind each.

## Internal-development skill

For work that changes NMP itself, use `skills/nmp-dev/SKILL.md`. It routes internal implementation, review, and testing work; for behavioral changes, test changes, or user corrections about how NMP should behave, start with `skills/nmp-dev/references/testing/INDEX.md`. `skills/nmp/SKILL.md` is the separate consumer-facing skill for applications that build with NMP; it is not authority for NMP internals.

## Issue-first, always — capture the why

**Every unit of work traces to a captured GitHub issue before it starts.** No silent side-quests, no code without a tracked reason. If you find work that needs doing and no issue covers it, *file the issue first*, then do the work; the PR references it and closing it is how the tracker stays honest (`docs/known-gaps.md` and a closed issue are the two ways "done" is recorded — mark done by removing it from the open set, don't leave finished work open).

The issue must **capture the why**, not just the what:

- State the problem or the goal in terms of a **consequence** — what breaks, what a user can't do, what invariant is unproven — not merely the mechanical change.
- **Anchor to higher-level thinking where it genuinely exists.** Link the VISION principle (P-number), the bug-class-ledger entry, the design doc, or the milestone the work serves. A change that closes a structural bug class or advances a milestone should say so, with the reference.
- **Do not hallucinate a rationale.** If the honest why is small — "this is a plain bug," "this is mechanical cleanup," "this unblocks a clean clone" — say exactly that. A fabricated grand justification is worse than an honest small one. The test for a claimed higher-level reason: it must be citable in a doc or a prior decision, not invented to dignify the task.
- Prefer **one issue per coherent unit of work** (one PR closes it). Group into an **epic** issue when a milestone fans out into many units; the epic carries the thesis and a checklist of child units, each child issue carries its own local why and links back to the epic.

The point is that six months from now the tracker answers *why did we do this*, and the answer is either a real, referenceable line of thinking or an honest "it was a bug" — never a confabulation.

## Standing conventions — read before proposing a surface change

Five rules that are not negotiable and are violated most often in *proposals*, not in code. Full reasoning, worked examples, and the incidents behind each live in `docs/internals/conventions/`.

1. **No backwards compatibility, ever** (`conventions/no-backwards-compatibility.md`). A replaced spelling is DELETED in the same change — no alias, no deprecation, no wrapper, no "keep both until X". **NMP has no external consumers**: every caller is in this workspace or a sibling that moves with it, so compatibility is a tax paid to strangers who do not exist. Where compatibility and architectural cleanliness conflict, **clean architecture wins absolutely**. Do not offer "replace vs wrap" as an option, and do not weigh breaking Swift/Kotlin/snapshots as an argument against a better design.
2. **Bech32 only at the user boundary** (`conventions/bech32-boundary.md`). `npub`/`nevent`/`naddr` exist to show something to a human or to accept what a human pasted. Everything internal — parameters, fields, FFI arguments, protocol-crate signatures — uses the decoded type (`PublicKey`, `EventId`). An app decodes at its own boundary and hands NMP a key.
3. **No invented categories, no repo jargon** (`conventions/naming-no-invented-categories.md`). Do not name a category the protocol does not have, and do not let internal shorthand harden into vocabulary. The worked example: "foreign kinds" described a category that does not exist in Nostr, hardened across 13 sites, and became load-bearing in a CI gate before it was removed (#960).
4. **No hidden runtime feature flags** (`conventions/no-hidden-runtime-feature-flags.md`). Requested behavior runs on the normal path; if it is not ready, it is not ready to merge. Runtime gates require an explicit staged/optional product decision. Cargo features and real configuration are unaffected.
5. **Name the behavior, or don't build the mechanism.** Before adding an abstraction, extension point, runtime mutation, durable table, worker, registry, or package, name the current user or protocol behavior that becomes impossible without it. If deleting the proposed mechanism would only remove hypothetical flexibility, do not add it — "someone may need this later", "third parties could implement it", "another subsystem uses the same pattern", and "an issue already describes the scenario" are all the same non-answer. Start from the smallest design that intentionally *refuses* unsupported scenarios; do not start from the most general architecture and then simplify its implementation. Exhaustively handle the edge cases of **supported** behavior — never widen the contract so a hypothetical's edge cases can be handled. An existing issue is evidence of prior reasoning, not authority. The two worked incidents: `EventStore` was a 17-method trait with one implementor and no `dyn` use (#1495), and the replaceable materializer grew runtime registration, replacement generations, detached threads, panic translation and a completion-correlation map around trusted compiled code that ships in the same binary (#1624). Both were correct implementations of a capability nobody had asked for.

## Architecture review gates

Five gates applied to every PR, encoding the type-over-convention doctrine (`docs/bug-class-ledger.md:3-5`: a bad path must be excluded by a type/API mechanism plus a falsifier, never by prose or reviewer memory; `docs/VISION.md:23-31`: the app-facing model is exactly two nouns). Full rules, trained tells, and the real incidents behind each live in `docs/design/architecture-review-gates.md`. Run 1–4 by eye against the diff; 5 has no mechanical check to run — it is a known gap (see below).

1. **Noun Gate** — a new public type must justify itself against extending an existing noun. Tell: *if your new type must forbid a field of the type it wraps, you have two owners of one property* (`HistoryQuery` rejecting `LiveQuery.selection.limit` was the confession; #485 folded it back into `observe(query, window)`).
2. **Reachability Gate** — every FFI-crossing error variant must cite a reachable construction site. Tell: grep the constructor; zero non-test call sites (History\* shipped `NoBoundary`/`WrongVersion` constructed nowhere).
3. **Bool-Lifecycle Gate** — an ownership/lifecycle `bool` adjacent to a handle or read inside `Drop`/`deinit`/`close` demands an enum, RAII, or `Option::take` — or a written justification (`owns_executor`, `AsyncWait.armed`). The same rule covers secret availability: a zeroizing secret must be owned by an RAII type that wipes on `Drop`, never tracked by an adjacent `is_zeroized`-style flag, and never mirrored by a second copy the flag doesn't cover (#765 shipped a wiped duplicate while the real operational secret lived on untouched).
4. **Destructive-API Gate** — a destructive verb enforces its precondition via typed refusal, never doc-only, and the refusal must survive to the FFI/Swift/Kotlin surface. Tell: find the "The caller must ..." doc sentence, then check what mechanically stops a violator — at every layer (`reset_persistent_store`/#489: the precondition had already vanished by `Engine.swift`).
5. **Cross-SDK Parity — invariant real, mechanism absent.** An app on one platform must not silently lose an operation the other two have. Nothing currently proves this: the old `check-sdk-parity.sh` compared lowercase word bags over whole files including comments and string literals, and passed a Swift SDK consisting of one comment file with the entire NIP-02 follow API deleted (#1637). It is gone rather than left green, because a check that passes while its invariant is violated is read as evidence. The replacement is a checked-in manifest of exported UniFFI items generating a protocol each SDK must conform to, so a missing `unfollow` is a compile error rather than a substring search. Until that exists, this gate is a **known gap**, not a passing check.

## Working discipline

- **Branches + PRs, never push work straight to `master` from a shared build.** Agents work in isolated git worktrees; a cohesive feature is one PR in one shared worktree.
- **Truth and honesty are the anchors.** The README is the current honest picture, not a pitch, and not a changelog. `docs/known-gaps.md` must list what doesn't work. Compiles ≠ works — verify the running result.
- **Fix end-to-end.** No temporary hacks, no compat aliases, no narrating a defect instead of fixing it. If a change is right, make it and update every caller in the same PR.
- **Test scope:** run the tests for the crates you touched (`cargo test -p <crate>`); a workspace run is the merge-time gate, not the per-change loop.
- **Hand off out loud.** Before ending a session or handing off in-progress work — a git worktree left behind, a blocker punted to someone else, a PR not yet merged — leave a clear handoff note on the owning GitHub issue/PR itself: the exact branch/worktree name, the current blocker, and the next step. This is the required, universally-reachable mechanism.
