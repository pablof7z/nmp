# Architecture review gates

- **Status:** active PR-review checklist (gates 1-4). Gate 5's invariant is
  real — an app on one platform must not silently lose an operation the other
  two have — but it currently has no mechanical check. Its previous check,
  `scripts/check-sdk-parity.sh`, compared lowercase word bags over whole
  files including comments and string literals, and still passed with the
  Swift SDK reduced to a single comment-only file (#1637). It was deleted
  rather than left green; the replacement is tracked in #1637 and not yet
  built.
- **Origin:** issue #496, written after the #485 architectural sweep. History\*
  (issue #474/#484) was a parallel-noun modeling error caught by a manual
  architectural read before merge. `Engine::reset_persistent_store` (issue
  #233, the precondition loss documented in #489) was **the same violation
  class and merged uncaught**. The conclusion the sweep reached (issue #490):
  *"The systemic failure ... [is] load-bearing invariants carried by
  convention — prose, a bare `bool`, a stringified error, a hand-copied field
  cluster, reviewer memory — instead of by a type/API mechanism."* These gates
  are the durable fix: cheap, proportionate checks applied at review time
  instead of relying on the next sweep to catch the next instance.

## Doctrine anchors

Every gate below reduces to one of two sentences this repo already committed
to:

- `docs/bug-class-ledger.md:3-5` — *"A bug class is closed only when a
  type/API mechanism makes the bad path unreachable and a falsifier
  demonstrates that fact."*
- `docs/VISION.md:23-31` — the app-facing model has **two nouns** (live query,
  write intent); diagnostics is a proof surface over both, *"not a third
  command surface or optional debug mode."*

## How to use this checklist

Run gates 1-4 by eye against the diff of any PR that adds a public type,
error variant, destructive verb, or a `bool` living next to a handle/executor.
They are cheap — each is a single question with a yes/no trained "tell."
Gate 5 has no mechanical check today (#1637): the by-eye read is not a floor
under some mechanical check, it is the entire check. Treat that as a known
gap, not as a checked box — see the gate's own section below for what was
tried, why it was deleted, and what the replacement needs to be.

---

## Gate 1 — Noun Gate

**Rule.** A new public type must justify itself against extending an
*existing* noun. NMP's app-facing model is deliberately exactly two nouns
(live query, write intent) plus diagnostics as a proof surface over both — see
`docs/VISION.md:23-31`. A third public noun is a design decision that needs to
survive scrutiny, not a default.

**Trained tell.** *If your new type must forbid a field of the type it wraps,
you have two owners of one property.* A rejection at construction time for a
field the wrapped type already owns is the compiler-shaped confession that
the new type and the old type are fighting over the same piece of state.

**Doctrine anchor.** `docs/VISION.md:23-31` (two nouns, not a third command
surface).

**Real example — the `History*` family (issues #474/#484/#485).** #474/#484
shipped a parallel noun: `HistoryQuery` / `HistorySubscription` / `HistoryBatch`
/ `HistoryContinuation` / `HistoryLoadFact` / `HistoryLoadError` /
`HistoryQueryError` / `HistoryAdvance`, alongside the existing live-query noun.
The #485 review reversed this: *"windowing is a policy on the single read
noun. `observe` gains an optional window parameter; `History` is not a public
noun."* The tell was structural, not stylistic — `HistoryQuery` had to reject
`LiveQuery.selection.limit`-shaped inputs that a plain query already accepts,
because bounded/windowed delivery is a *capability* of the same aggregate
(`HistoryState` ran the identical reactive path as a plain handle:
`try_apply_committed_history_row_changes` mirrored
`try_apply_committed_row_changes`), not a second aggregate. Two nouns forking
on one legal-combination-each is the modeling error; the fix folds the
capability back into `observe(query, window)`.

---

## Gate 2 — Reachability Gate

**Rule.** Every FFI-crossing error variant must cite a reachable construction
site. An error the type system allows an app to match on, but that nothing in
the codebase can ever construct, is dead weight that misleads callers into
handling a case that cannot happen (or, worse, hides that a real case is
*not* handled).

**Trained tell.** Grep the variant's constructor uses. Zero non-test call
sites, or call sites that are provably unreachable through the supported
runtime path, is the tell.

**Doctrine anchor.** `docs/bug-class-ledger.md:3-5` (a bad path must be
excluded by mechanism, not merely unencountered by luck — an error variant
that models a path nothing can reach is the mirror-image failure: it *looks*
like a proof of exclusion but proves nothing).

**Real example — `History*`'s dead error variants (#485).** The #485 review
found *"Four of ten `HistoryLoadError` variants are also dead/unreachable
through the supported API (`NoBoundary` never constructed; `WrongVersion`/
`WrongDescriptor` unreachable in-process; `LoadInProgress` unreachable via the
single-iteration runtime)"* — and separately confirmed that on `master` at
sweep time, *zero* dead error variants were actually FFI-pinned; the
dead-variant surface only existed on the unmerged `History*` branch. That is
exactly the gate working as intended: caught before merge, not after.

---

## Gate 3 — Bool-Lifecycle Gate

**Rule.** An ownership/lifecycle `bool` adjacent to a handle, or read inside
`Drop`/`deinit`/`close`, demands an enum, RAII wrapper, or `Option::take` —
or an explicit written justification for why a plain bool is safe here.

**Trained tell.** Search for a `bool` field whose only reader is a
`Drop::drop`/`deinit`/`close` body, gating whether that teardown path
actually releases a resource. A plain bool can be constructed in a
combination the author never considered (double-drop, drop-after-move,
racing construction), silently skipping or double-running the teardown it
guards.

**Doctrine anchor.** `docs/bug-class-ledger.md:3-5` — the mechanism has to
make the bad state (teardown running twice, or not at all) unrepresentable;
a bool next to `Drop` only makes it *unlikely*.

**Real example — `ResolvingOwner.active` (`crates/nmp-ffi/src/auth.rs`).**
`ResolvingOwner` carries `sender: Option<nmp::AuthPolicyPendingSender>` plus a
plain `active: bool` (`auth.rs:85-90`), constructed `true` (`auth.rs:102`) and
flipped to `false` only by `finish()` after it has already driven completion
state to `Completed` and notified waiters (`auth.rs:113-126`). `Drop for
ResolvingOwner` re-runs that same completion/notify sequence, but only `if
!self.active { return; }` skips it (`auth.rs:129-133`). Nothing in the type
system stops a future call site from cloning/relocating the flag out of step
with which completion path actually ran, or from adding a second early return
that forgets to flip it — the invariant that `finish()` and `Drop` never both
fire the completion sequence lives entirely in the two call sites agreeing
with each other, not in the type. The two original instances issue #490's
ledger cited under "replace 3 bare-`bool`-`Drop` gates" — `owns_executor` in
the deleted NIP-46 signer session, and `AsyncWait.armed` in the pre-#827
`nmp-engine` crate — are both gone (the former crate was removed with NIP-46
in #1171, the latter folded into `nmp` and its bool replaced by
`FlightWaitLifecycle`, an enum, in `crates/nmp/src/relay_information_service.rs`
— itself a worked example of this gate's fix applied). `ResolvingOwner.active`
is this gate's current trained-tell example: a plain bool, not an enum or
`Option::take`, still gating a `Drop` teardown path.

---

## Gate 4 — Destructive-API Gate

**Rule.** A destructive verb (delete, reset, remove, revoke) must enforce its
precondition via typed refusal, never doc-only prose — and that enforcement
must survive to *every* SDK surface (FFI, Swift, Kotlin), not just the Rust
call site closest to the mechanism.

**Trained tell.** Read the destructive function's doc comment for a sentence
starting "The caller must ..." — then check whether violating that sentence
is a compile-time impossibility, a runtime typed error, or nothing at all.
Then re-check the same question at the Swift/Kotlin surface specifically:
preconditions are disproportionately likely to be softened or dropped
crossing that second boundary, because it's a second hand-rewrite of the same
doc comment.

**Doctrine anchor.** `docs/bug-class-ledger.md:3-5`, applied at review time
instead of at falsifier-writing time.

**Real example — `reset_persistent_store` (issue #233, merged uncaught; fixed
under #489).** `Engine::reset_persistent_store` (`crates/nmp/src/engine.rs:119-127`)
deleted the store file with no open-handle check, no lock, no registry — an
unguarded `std::fs::remove_file`. The precondition existed only as a doc
comment (`engine.rs:117`: *"The caller must shut down and drop every engine
using `path` before invoking this operation."*). It then degraded further
crossing each boundary: `crates/nmp-ffi/src/facade.rs:307-310` forwarded the
call with no precondition doc at all, and — the sharpest instance —
`Packages/NMP/Sources/NMP/Engine.swift:64-70`'s docstring softened the
requirement to *"Destructively remove one **closed** persistent NMP store,"*
asserting "closed" as if it were a checked adjective instead of an
unenforced caller obligation, with the Kotlin surface (`Engine.kt:53-54`)
surfacing no precondition whatsoever. redb takes no OS-level file lock, so
there was no accidental safety net underneath the prose either. #489's fix is
the gate's remedy pattern: a typed `EngineError::StoreStillOpen { path }` /
`FfiError` variant from an in-process open-path registry, so the bad call is
mechanically refused instead of merely discouraged, at every layer.

---

## Gate 5 — Cross-SDK Parity (mechanical CI check)

**Rule.** The Swift and Kotlin public inventories must not diverge from the
Rust FFI export surface. A feature landing in Rust+FFI+one native SDK but not
the other is a silent, structural gap in exactly the surface the repo claims
parity over.

**Trained tell.** A PR that touches `crates/nmp-ffi/src/**` and one of
`Packages/NMP/Sources/**` or `Packages/NMPKotlin/src/main/kotlin/**`, but not
both.

**Doctrine anchor.** `docs/VISION.md`'s two-noun app-facing model is a claim
about what apps on *every* supported platform can do; a platform silently
missing a feature the model claims to provide is a parity break, not a
scoping choice, unless explicitly documented as platform-specific.

**Real example — NIP-02 Following absent from Kotlin (issue #493).** The
NIP-02 following surface shipped fully for Swift (`Packages/NMP/Sources/NMP/Following.swift`,
391 lines) and in the Rust FFI (`crates/nmp-ffi/src/nip02.rs`,
`crates/nmp-ffi/src/facade.rs:489-531`) but **zero** matching Kotlin surface
existed — silently breaking the claimed Rust/UniFFI/Swift/Kotlin parity for
the largest single instance of this class the #485 sweep found.

**Mechanical check — deleted (#1637).** `scripts/check-sdk-parity.sh`
tokenized the lowercase concept-words of every `#[uniffi::export]`ed
function/method and every `uniffi::Object`/`Enum`/`Record`/`Error`-derived
type in `crates/nmp-ffi/src/*.rs`, and diffed that word set against the
identifier vocabulary found *anywhere* in `Packages/NMP/Sources/**` and
`Packages/NMPKotlin/src/main/kotlin/**` — including comments and string
literals, and blind to a `#[cfg]` sitting between a derive and the
declaration it applies to (so `FfiSimpleGroupEntry`
(`crates/nmp-ffi/src/types.rs:492`), `FfiSimpleGroupsList` (`:507`), and
`FfiReaction` (`:730`) were never visible to it at all).

Mutation testing on a scratch copy of master found it had no falsifier:

- deleting the entire public NIP-02 follow API
  (`Packages/NMP/Sources/NMP/Following.swift`, 385 lines) → **passed**
- also deleting `NIP22.swift` and `NostrEntity.swift` → **passed**
- deleting all 25 hand-written Swift files (5,758 lines) → failed, 161
  concepts missing
- adding back **one** 1,251-byte file containing two comment lines and zero
  Swift declarations, listing those 161 words → **passed**

Its allowlist, `scripts/check-sdk-parity-allowlist.toml`, stayed empty of
entries for the entire time the check existed. That read as "no exceptions
needed"; it actually meant the check was never strong enough to produce a
failure worth allowlisting.

A check that passes while its invariant is violated is a false assurance —
green gets read as evidence by everyone except the person who found the
mutation. Both files, and the `sdk-parity` job that ran the script in
`.github/workflows/architecture-gates.yml`, were deleted in the same change
(#1637) rather than left green.

**Historical backtest (of the deleted script).** Before it was deleted, the
script *was* backtested against real history: at the commit before the #493
Kotlin Following port (`920033e^`), it failed with the five real
missing-concept words (`follow`/`following`/`unfollow`/`relationship`/
`availability`); on post-#493 masters it passed. That the check could catch
that one historical case is why it shipped originally — the mutation testing
above is what later proved it could not catch a far larger deletion of the
same kind.

**Replacement — not yet built (#1637).** The target mechanism is a
checked-in manifest of exported UniFFI items generating a protocol/interface
each SDK must conform to, so a missing `unfollow` is a compile error, not a
substring search. Until that exists, gate 5 is enforced only by the by-eye
read described in "How to use this checklist" above.

**CI wiring.** None. The `sdk-parity` job was removed from
`.github/workflows/architecture-gates.yml` in the same change that deleted
the script (#1637).
