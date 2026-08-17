# The Canary

One small, real application whose job is to keep NMP honest as a product.

The Canary is not a reducer harness, a mock-heavy integration test, an SDK
showcase, or an architecture demo. It behaves like an ordinary downstream
application and uses only the APIs an actual application is expected to use.
It answers, continuously and empirically:

> Can a normal application actually use NMP to read, publish, recover,
> reconnect, and remain fast and understandable under realistic conditions?

It exists because every other layer — owner tests, headless falsifiers, BDD,
SDK tests — can be green while the public API is awkward, an advertised flow
does not work end to end, restart breaks only with real resources, relay work
explodes, resources are pathological, a capability needs app-side lifecycle
hacks, or an internal concern leaks into application state.

It lives at `apps/Canary`, evolved from `apps/Falsifier` rather than built
beside it. That decision and its evidence are in [Why Falsifier became the
Canary](#why-falsifier-became-the-canary).

## Non-negotiable realism

The Canary uses: the supported public NMP/Swift API; the real FFI/native
library; the real runtime; the real Redb persistence path; real OS
threads/tasks; real TCP and WebSocket connections; real Nostr frames; real
signatures from a real local signer; real process shutdown and restart; and
**real relay implementations launched as separate processes**.

It does not use: `nmp-engine`, `nmp-runtime`, `nmp-store`, router/resolver
internals, testkits, `@testable import NMP`, fake engine or store
implementations, direct Redb inspection to decide whether the app is correct,
privileged commands a production application cannot issue, or any test-only
constructor that bypasses normal lifecycle rules.

The app must not know whether NMP internally uses a query engine, a publish
engine, request attempts, coverage maps, or lanes. **If a Canary flow cannot
be implemented without that knowledge, that is an NMP API finding, not a
Canary problem.**

Treat awkward Canary code as a product bug until shown otherwise, and do not
add helper abstractions inside the Canary to conceal an awkward NMP API. A
little duplication is preferable to hiding evidence.

## The relay lab

The deterministic local lab launches real relay binaries as child processes
with isolated temporary data directories. The app talks to them only over real
network sockets.

### strfry, verified

**strfry** is the lab's relay, chosen by building and running it rather than by
reading its documentation. Verified on macOS/arm64 on 2026-08-16:

- **Real process, real persistence.** Seed one event over a real `EVENT`
  frame, `kill -9` the process, restart against the same LMDB directory, query
  it back: the event survives, with `data.mdb`/`lock.mdb` on disk. This is the
  proof no in-process relay can pass.
- **NIP-77 negentropy is genuinely implemented.** Two independent strfry
  processes seeded with different events reconciled bidirectionally — real
  negentropy log output, both sides converging. Not a stub.
- **NIP-42 write denial is real**, with a machine-readable
  `auth-required: this relay requires NIP-42 auth for writes`. Fresh challenge
  per connection confirmed in `RelayIngester.cpp`: a new `challengeGenerator`
  is minted per connection id, which is exactly the reconnect property the AUTH
  scenario needs.

Rejected alternatives: **nostr-rs-relay** has real NIP-42 but no NIP-77 at all
— its NIP support table never mentions it — so it cannot serve the
reconciliation scenario. **khatru** is a library rather than a relay binary
(you write your own `main.go` and wire up storage yourself), and its NIP-77
completeness is unverified; more integration work for no proven advantage.

### Why `ScriptedRelay` cannot be the system under test

This one matters, because `crates/nmp-test-support`'s `ScriptedRelay` looks
like a candidate and is not.

It is a real WebSocket on a real port, and its `nostr-relay-builder` backend
is a genuinely real negentropy implementation. But it runs **inside the same
OS process** as its caller and has **no database file**: its "restart" is a
fresh in-memory instance on the same port. It cannot prove that anything
survives a restart, because there is nothing on disk to fail to read back.
That is a property of its design, not a bug in it.

`ScriptedRelay` remains the right fast fixture for unit and BDD tests. It is
disqualified as the Canary's system under test.

### The loophole to watch for

NMP already contains a working pattern for wrapping `ScriptedRelay` in a thin
binary and invoking it as a subprocess:
`crates/nmp-test-support/src/bin/outbox-routing-relay-harness.rs`, used by the
Android qualification suite. A contributor could reuse that pattern in good
faith for the Canary and produce something that satisfies the *letter* of
"separate process" while being exactly the in-process, non-persistent fake
this document forbids as the system under test.

**Review must check what is inside the process — does it persist across
kill+restart, is it a real third-party relay codebase — not merely that a
second PID exists.**

### What is enforced, and what is only reviewed

Living in-repo means the Canary loses guarantees a separate repository would
have had for free. Worth naming, because this is where erosion starts.

Enforced by construction:

- The relay is reached only by a `ws://` URL to a separate process. No
  compile-time or link-time edge exists from the Canary's Swift targets to
  `nmp-test-support` or any Rust internals; the relay binary has no dependency
  edge into NMP at all.
- Seeding happens over real `EVENT` frames, so there is no direct
  insert-into-the-database door on the relay side to be tempted by.

Policed by review only:

- Nothing prevents the Canary's Swift targets from reaching past the public
  `NMP` facade into FFI internals, or from adding `@testable import NMP`. A
  separate repo consuming a published package could not do this; an in-repo app
  can.
- Nothing prevents a scenario author from asserting against the relay's
  exported database instead of driving the app through the public facade.
- The harness loophole above.

### Where the controller lives, and how the scenarios run

Three SwiftPM packages, not the Xcode project. `apps/Canary/RelayLabKit` is
the relay-lab controller itself — start, stop, kill, restart, partition,
heal, seed, ephemeral port, isolated temp directory, bounded-poll readiness
rather than sleeps, a one-shot real-TCP reachability probe (`isReachable`,
what "the relay is genuinely unreachable" has to mean), and an optional
shared `dataDir` so a second relay process can write into a stopped relay's
durable store on its own port — C13's outage window. It knows nothing about NMP; it is a generic real-relay
lifecycle library, reusable outside this app entirely. It is consumed by two
thin CLI targets a developer runs directly (`swift run relay-lab-lifecycle
<strfry-binary>`, `relay-lab-nip42`) to bring the lab up and drive it by
hand, and by `apps/Canary/CanaryScenarios`, a sibling SwiftPM package that
depends on both `RelayLabKit` and the local `NMP` package — the one place
that is both NMP-aware and relay-lab-aware, which is exactly what a scenario
has to be. `apps/Canary/setup-strfry.sh` builds the pinned relay binary
alongside `RelayLabKit`.

**`swift test` from `apps/Canary/CanaryScenarios` is the whole entry point
for running scenarios** — no `xcodegen`, no `xcodebuild`, no simulator, no
Xcode project involved anywhere in that path. This was a deliberate
correction: the scenarios first landed as an Xcode test target
(`CanaryRelayLabTests`), which worked, but made "run the evidence" cost an
`xcodegen generate` plus an `xcodebuild test` invocation — heavy and
Xcode-shaped for what is supposed to be the cheap, constant source of truth.
They moved to plain SwiftPM for exactly that reason: an agent or a person
with nothing but a Swift toolchain runs one command, repeatedly, for free.

macOS only, and this has not changed: `RelayLabKit` spawns the relay via
`Foundation.Process`, which the iOS SDK does not expose at all, device or
simulator — there is no way to launch an arbitrary child process from code
built against the iOS SDK. `NMP`'s own `Package.swift` already declares
`macOS(.v13)` alongside `iOS(.v16)`, so `CanaryScenarios` drives the
identical public `NMP` module the iOS `Canary` app links, built for the host
platform instead of the simulator — the app's real read/write path, not a
different one. The iOS `Canary`/`CanaryTests` Xcode targets are unaffected
and unchanged by any of this; they are not on the scenario-running path and
never were meant to be.

**Two one-time prerequisites**, both fail loudly and by name if skipped
rather than producing a confusing failure (see
`apps/Canary/CanaryScenarios/README.md` for the full detail):

- The `NMP` xcframework must exist (`Packages/NMP/NMP.xcframework`,
  gitignored, not committed). `swift test` fails immediately with a missing
  binary-target error if it does not. Build it once from the repository
  root: `scripts/build-swift-xcframework.sh --macos-only` — the
  `--macos-only` mode, not the `--sim-only` this repository's other Swift
  consumers use, because nothing here ever runs on an iOS simulator or
  device. Documented as "a few minutes on a cold `cargo` cache"
  (`Packages/NMP/README.md`); measured on a warm cache in this repository at
  2-20s for the Swift/link half once the Rust artifacts already existed.
- `strfry` must already be built: `apps/Canary/setup-strfry.sh`. Each
  scenario locates it at `$RELAY_LAB_CACHE_DIR` (default
  `~/Library/Caches/nmp-canary-relay-lab`) itself and calls `XCTSkip` by
  name if it is missing — proven by pointing `RELAY_LAB_CACHE_DIR` at a
  nonexistent path and confirming both scenarios report `Test skipped -
  strfry is not built at <path> -- run apps/Canary/setup-strfry.sh first`
  rather than crashing or hanging.

Once both exist, `swift test` from `apps/Canary/CanaryScenarios` measured at
~20s on a cold local SwiftPM build cache (prerequisites already built) and
~3s fully warm — genuinely cheap enough to run after every change.

The relay binary itself is built by a documented, commit-pinned script into
a gitignored cache directory outside the repository entirely. It is **not**
vendored: a committed platform-specific binary is dead weight in git history
forever, and a build script is small text that reproduces it on demand.
Note honestly that a clean machine also needs the Homebrew dependency line,
which costs real time beyond the strfry build itself.

## Two environments

**Deterministic local lab** — authoritative acceptance evidence. Reproducible
from a seed, fails loudly. No scenario may be skipped because the network was
unavailable.

**Public-relay mode** — the same unmodified app pointed at ordinary public
relays, for reconnaissance: strange server behaviour, latency distributions,
reconnect storms, large existing datasets, real NIP-11 behaviour, long-lived
resource behaviour. This is not a deterministic pass/fail oracle. External
failure is reported as observed evidence, never silently converted into a
successful skip.

## Scenario status

C1 cold start and live feed; C2 cache then offline restart; C3 multi-relay
dedup and provenance; C4 reactive derived query; C5 replaceable, deletion and
stale redelivery; C6 deep windowing; C7 normal publish; C8 publish while
relays fail; C9 crash/restart during publication; C10 offline write then
convergence; C11 semantic capability; C12 identity freeze; C13 relay
disconnect/reconnect; C14 NIP-77 reconciliation; C15 NIP-42 AUTH; C16 slow
consumer and backpressure; C17 repeated lifecycle churn; C18 clean shutdown.

**C1 is proven live** against a real strfry child process:
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C1ColdStartLiveFeedTests.swift` seeds
one event over a real `EVENT` frame before the engine exists (empty store,
already-seeded relay), constructs `NMPEngine` normally, opens one
`engine.observe(NMPFilter(kinds:authors:))`, waits (bounded, no sleep-as-oracle)
for the historical row, then — from inside that SAME still-open subscription —
seeds a second event through the relay and waits for it to arrive live with no
duplicate canonical row (`Set(rows.map(\.id)).count == rows.count == 2`).
Passed in ~1s. Deliberately sabotaged (pointed the filter at a wrong-but-valid
64-hex pubkey) to confirm it is a real falsifier, not a vacuous pass: it failed
red with the exact expected timeout message rather than passing regardless of
relay behaviour, then was restored and re-confirmed green.

**C7 is proven live**, the write path's first real end-to-end exercise:
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C7NormalPublishTests.swift` opens a read
query before publishing, then `engine.publish(WriteIntent(...))`s a real
signed event under a real local-key account through `.explicit(relays:)` to
the same strfry process. Two independent consuming tasks (one over the
already-open read query, one over `receipt.status`) run concurrently, sharing
only the one cross-cutting fact that matters: whether the row was visible
before this relay's confirmation reached the receipt stream. It was, every
run. The row's `sources` then grow to include the relay once its echo
arrives — the SAME canonical row gaining provenance, never a second row — and
the row query and the receipt stream independently agree on the exact event
id. Terminal outcome reached `.settled`. Passed in under 1.5s.

Falsified three ways, each restored afterward: routing the write to a dead
address (`ws://127.0.0.1:1`) timed out with the expected message, since the
echo can never arrive if the write never reaches the relay; inverting the
acceptance-visibility assertion failed with the real captured value
(`Optional(true)` vs. the deliberately wrong `Optional(false)`) rather than
passing regardless; inverting the echo/dedup assertion failed showing the
real `sources` array containing the relay URL. All three prove the scenario's
assertions are live and would catch a real regression in any of these three
independent ways, not just the one that happened to get exercised.

No API finding this time — `WriteIntent`, `Receipt`/`ReceiptStatus`, and the
local-acceptance-then-echo behavior all worked exactly as documented, with no
app-side polling or retry required anywhere in the scenario.

**C9 is proven live** across all three of the contract's kill points, against
a real `kill -9` of a real separate OS process — not an in-process `Engine`
drop, which proves almost nothing about crash safety (ordinary Swift cleanup
still runs).
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C9CrashDuringPublicationTests.swift`
spawns a sibling executable, `canary-c9-publisher`, that constructs an engine
over a store path handed to it, adds and persists a local-key account
(needed so the SAME signer exists again after restart), publishes one real
signed event, prints machine-readable
stdout markers as it reaches each fact the parent needs, then parks doing
nothing further. The parent waits for a marker with a bounded timeout,
`kill -9`s the child for real, then opens a FRESH engine in-process over the
SAME store path with the persisted session restored, finds the obligation by
enumerating `publishQueue` — never an app-owned shadow ledger of NMP's own
durable queue (#1770) — reattaches by the discovered receipt id, and proves
recovery through the public API only:

1. **After local acceptance, before delivery completes** (relay reachable,
   killed as fast as possible after the acceptance marker — a real race
   against real network timing, not a deterministic setup). Obligation
   found by enumerating `publishQueue` and reattached with
   `reattachReceipt(id:)`; canonical row visible; delivery resumes and
   settles with no further app action; no duplicate row.
2. **While the relay is unreachable** (`RelayHandle.partition()` before the
   child ever starts, so nothing was ever sent — deterministic by
   construction, not a timing race). Same recovery proof, plus: healing the
   relay only AFTER the crash is what lets delivery complete (falsified
   below).
3. **After one relay succeeded and another did not** (two relays; the
   second partitioned from the start; the child watches its own
   `receipt.status` and prints its marker the instant the first relay
   reaches `.published`). After restart, the sharpest assertion: the relay
   that already succeeded is tracked for any regression from `.published`
   back to `.waiting`/`.sent` — none observed, across every run — while the
   still-pending relay (healed after the crash) completes normally.

   **What this proves, exactly.** No *app-visible receipt-state* regression.
   It does **not** prove no network resend: the oracle watches `receipt.status`
   only, so NMP re-sending the `EVENT` to the already-succeeded relay — which
   the relay would deduplicate — leaves the receipt at `.published` and the
   assertion green. Proving the stronger claim needs relay-side inbound EVENT
   counts, or a public NMP delivery-attempt fact. Neither exists yet.

All three passed on every run (4 consecutive full-suite runs, ~1s each) as
originally validated. Falsified three ways at that time, each restored
afterward: pointing the restarted engine at a different store path made
`reattachReceipt` correctly report `.notFound`; skipping the post-crash
`heal()` in case 2 made recovery
correctly fail to complete (the failure surfaced as "canonical state did
not survive" rather than a more precise "delivery never resumed" — an
honest imprecision in the test's own failure labeling, not a false
result, noted rather than smoothed over); inverting case 3's no-resend
assertion failed on the real captured `false` value.

Reused `RelayHandle`'s own process-control mechanism rather than a second
hand-rolled one: `RelayLabKit` gained a small shared `ChildProcess.killAndWaitForExit`
(SIGKILL + bounded poll for real exit), used both by the C9 harness for its
publisher child and available to `RelayHandle` itself.

No API finding — every step was expressible through `NMPEngine`,
`NMPSessionPayload`, and the write/reattach doors exactly as documented. The one thing worth naming precisely: a restarted
app that lost the account's session payload cannot resume a still-unsigned
write at all (there is no signer), which is why persisting the session
payload is not optional plumbing but a hard prerequisite for any of this to
work — exactly the case the shipped `AppModel`/Compose session persistence
already covers, but worth stating as a discovered fact here rather than an
assumption.

**Superseded (#1770), and the door is now deleted outright:** the paragraphs
above describe C9 as it was first proven, when the app minted its own
correlation token and the parent test handed that same token to the recovery
half by argv. That shape proved the
engine recovers a *known* obligation, not that an app can *find* one after a
crash — the actual half an app author has to build. `publishQueue` already
answered "what have I got outstanding" without an app-side ledger
(`Receipt.swift:184`), so both the shipped `AppModel` and this test's
recovery half now enumerate it instead: `assertRecovery` finds the obligation
via `publishQueue(limit:)` before reattaching by `reattachReceipt(id:)`, and
the Canary's own `UserDefaults` correlation ledger (`AppModel`/`ComposeView`,
built twelve days after `publishQueue` shipped) was deleted for the same
reason.

With no consumer left, `WriteIntent.correlation`, `CorrelationToken` and
`reattachByCorrelation` have since been removed from NMP entirely. An event's
id IS the hash of its contents, so the "legitimately re-composed draft (fresh
`created_at`)" the token's own doc named as its reason to exist is a DIFFERENT
event, not a modified one — and silently resolving it to the earlier obligation
discarded exactly what the app asked to publish.

Two facts about the starting position, established by survey:

- **The app has never called `publish`.** Six scenarios (C5, C7, C8, C9, C10,
  C12) have no product surface at all — not because the SDK lacks anything
  (`WriteIntent`, `Receipt` and `reattachReceipt` are complete and well
  specified) but because no screen uses them.
- **Session identity did not survive restart**, which silently blocked C2, C9
  and C12: there was no identity for a resumed write to remain frozen to. Both
  C9 and C2 are now proven through `NMPSessionPayload`, so the blocker is
  closed for those two; C12 remains unwritten.

**C17 is proven as an oracle, and two of its three phases pass. The third
fails, and the failure is a real finding (#1846), not a scenario defect.**
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C17RepeatedLifecycleChurnTests.swift`
is the first measurement of NMP's resource behaviour over time at any
layer — nothing anywhere previously watched memory, sockets, threads or
subscription count across repeated open/close cycles (#1844).

**Where the measurement lives, and why.** Footprint, open file descriptors
and live thread count are properties of a PROCESS; there is no per-object
equivalent. #1796 is this repository's standing proof that a process-wide
measurement inside a shared test binary is not an oracle — it cannot tell
"the subject misbehaved" from "something else in this process was busy",
and a leak oracle is exposed in both directions, since ambient allocation
manufactures growth NMP did not cause and ambient release hides growth it
did. So the churn runs in `canary-c17-churner`, a sibling executable whose
entire job is the churn: no XCTest runner, no other scenario, no relay
controller, no relay-lab dependency. The parent test starts the relay,
seeds one event over a real EVENT frame, and reads samples off stdout —
C9's parent/child split, for a different reason.

Four numbers per cycle: `phys_footprint` (what macOS charges the process),
`mstats().bytes_used` (byte-granular heap; Rust's default macOS allocator
IS the system malloc, so engine allocations land here), an exact file
descriptor count, and the live thread count — plus the one resource count
the public API exposes, per-relay `wireSubCount` from
`observeDiagnostics()`.

**PASSING — `repeat`, 300 open/close cycles of one identical filter
against one engine.** Everything returns to steady state: fds flat at 8,
threads flat at 18, `wireSubCount` back to 0 after every teardown,
heap +6 B/cycle with a decile series that is flat to within 5 KB across
300 cycles. Cross-length control: final heap 74,456,848 B at 300 cycles
and 74,456,400 B at 1200 — **−0.5 bytes per additional cycle**, i.e. no
per-cycle growth at all.

**PASSING — `engine`, 60 whole-engine construct/read/`shutdown()` cycles
over the same store path.** This is also C17's connect/disconnect churn:
each engine dials the relay fresh and each shutdown drops it, so a leaked
socket or engine thread per lifecycle would land in the fd and thread
series without a second control channel. Post-shutdown heap sits at
177 KB, flat, against the ~74 MB a live engine holds — so `shutdown()`
releases essentially the entire engine, and fds drain from 6 to 3.

**FAILING — `distinct`, 300 cycles with a different wire filter each
time.** Heap grows +402 B/cycle with decile means rising monotonically in
all ten deciles. Cross-length: 74,577,904 B at 300 cycles, 74,840,160 B at
1200 (+291 B per additional filter), 76,354,832 B at 4000 (+541 B per
additional filter) — linear, no plateau, and `phys_footprint` over that
4000-cycle run rose 9.49 MB → 13.78 MB. **NMP retains a few hundred bytes
per distinct filter ever observed for the lifetime of the engine, and
closing the observation does not release it.** Filed as #1846 and left
red on purpose: the threshold was not raised to make it green. Every other
resource is clean in this phase too — fds and threads flat, `wireSubCount`
back to 0 after all 4000 teardowns.

What C17 cannot determine: whether that retention is an unbounded
in-memory map or the store's page cache holding legitimately durable
per-filter coverage rows. The public API exposes no way to tell — that is
itself the API finding below.

**The thresholds are measured, not chosen.** The first draft asserted
16 B/cycle on the heap, malloc's smallest allocation quantum. Against a
~74 MB live-engine baseline that is below the instrument's noise floor,
and it failed the `repeat` phase, which the cross-length control proves
has zero per-cycle growth. The resolution was then measured — `repeat`
reported +22 B/cycle at 300 cycles and +41 at 1200 purely from noise — and
the committed bound set at 128 B/cycle, roughly three times the largest
drift observed on a series known to be flat. Stated plainly in the file:
C17 cannot resolve a heap leak smaller than about 128 B/cycle in the
single-engine phases. `phys_footprint` is bounded on total drift (2 MB)
rather than a rate, because it swings ±900 KB in both directions on runs
with no growth and dividing a fixed swing by the run length makes the same
behaviour read as −33,810 B/cycle at 60 cycles and −3,166 at 300.

**Falsified two ways, each restored afterward.** Retaining every `NMPQuery`
instead of releasing it (a leaked subscription) failed red on three
assertions with the real numbers: heap +22,592 B/cycle against a clean
+22, footprint +22,518 B/cycle, and `wireSubCount` after close stuck at 1
with `DRAINED:` reporting 1 instead of 0. Leaking one file descriptor per
cycle (`open("/dev/null")`, never closed) failed red at exactly
**+1.00 fd/cycle**, first sample 9, last sample 68 over 60 cycles, while
every other series stayed flat — the two falsifiers hit different
assertions, so neither is carrying the other.

A third falsification arrived unplanned and is the more useful one. The
first draft ended each cycle at the query's first delivered batch, which
looked reasonable and was wrong: the first batch carries `rows=0` and
arrives from the local store immediately, long before the subscription
reaches the relay (measured: three consecutive `rows=0` batches precede
the `rows=1` one). Every cycle was tearing the observation down before it
was ever established. The scenario reported this as `0/300 cycles saw a
live wire subscription while open` and refused to treat the post-teardown
zeros as evidence — the liveness assertion exists precisely so a count
that is always zero cannot pass as a released resource. Cycles now end on
the engine's own report that the subscription is established.

**API findings.** Two, both small and both real.

`observeDiagnostics()` is push-only: there is no synchronous "what is your
current snapshot". An app that wants a point-in-time resource reading must
hold the stream open and cache the last value, which is what the churner
does and what the shipped `NMPDiagnosticsSnapshotObserver` sugar does. Not
a defect, but it means "read the engine's current resource state" is not a
call an app can make.

More consequentially, `DiagnosticsSnapshot` reports per-relay subscription
counts, wire filters and coverage state, but nothing about retained
per-filter bookkeeping or memory — so an app hitting #1846 has no
public surface that would tell it what is growing, and neither did this
scenario.

**C13 is proven live** (#1863), and it is also this suite's first exercise of
**two concurrent observations sharing one query** — the shape nothing else in
this repository covers, because the scale tests all use one handle per key,
which is exactly why they missed #1848 (a shared-demand lifecycle defect where
a demand was never closed).
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C13RelayDisconnectReconnectTests.swift`
opens two `NMPQuery` handles over the IDENTICAL filter, consumes both in
independent tasks for the whole scenario, and drives them through a real
strfry process that is `SIGKILL`ed and brought back on the same port over the
same LMDB directory. Six phases, ~8-10s per run, stable over six runs:

1. **Genuinely live**, asserted not assumed: both observations were delivered
   the seeded event AND `observeDiagnostics()` reports **exactly one** wire
   subscription for the two of them. That exact `1` is what makes "sharing"
   true rather than two independent demands wearing the name; without it
   phases 5-6 would prove nothing about sharing at all.
2. **Genuinely severed**: the port stops accepting a real TCP connection
   (`RelayHandle.isReachable`, new in `RelayLabKit`) and both observations'
   own `SourceStatus` walks `finishedStoredEvents` → `disconnected`.
3. **The event really arrived during the outage.** A second strfry process,
   on its own ephemeral port over the SAME LMDB directory
   (`RelayHandle(dataDir:)`, new), writes it over a real `EVENT` frame with a
   real `OK` while the app's port is provably dead. Deterministic by
   construction — the app was dialing a port with nothing behind it and has
   never been told the sidecar's port exists — rather than a race won.
4. **Both sharers resume** with no app action whatsoever: no reopened query,
   no new engine, no app-side retry. Exactly the two ids, once each, in both,
   checked against the latest snapshot AND the union of everything either ever
   saw, so a row that appears and then vanishes is a loss rather than a pass.
5. **Closing one sharer leaves the other working** — a third event published
   after `sharerOne.cancel()` still reaches sharer two, and the closed one
   never receives it.
6. **Closing the last sharer releases the wire** — `wireSubCount` 1 → 0.

Falsified four ways, each restored and re-confirmed green. Never restarting
the relay left both sharers holding only the pre-outage id with status `error`.
Never closing the last sharer left `wireSubCount` stuck at `1` after a bounded
15s wait — #1848's own defect, caught. Closing BOTH sharers before the third
publish made the survivor miss it. **The fourth is the one that matters most**:
publishing the "outage-window" event BEFORE the outage instead of during it
leaves phase 4 fully green — `bothSawDuring=true` — and ONLY the phase-3
precondition catches it. That is precisely the vacuous reconnect proof this
scenario would otherwise have been, and it is now mechanically ruled out.

**C13's API finding.** `RelayDiagnostics.wireSubCount` does **not** fall to
zero while a relay's socket is dead: measured at `1` for the whole outage with
the relay row still present in the snapshot, and `transportDegraded` `nil`
throughout. It counts subscriptions the relay is *planned* to hold, which is
honest while NMP retries — but it means the engine-global diagnostics stream
cannot answer "is this relay's subscription established right now", and the
only public fact that can is per-QUERY `SourceEvidence.status`, which requires
an app to already be holding a query. An app watching only `observeDiagnostics()`
sees no difference between a healthy relay and a dead one. That gap is #755's
subject. The scenario's first draft asserted the zero; the assertion was
**removed rather than inverted**, and the number is printed on every run so
neither value is silently promoted to a contract.

**C2 is proven live** (#1864) — the headline local-first claim, which had no
end-to-end evidence anywhere until now.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C2CacheThenOfflineRestartTests.swift`
runs the ONLINE half in a sibling executable, `canary-c2-warmer`, that signs in
with a real local-key account, reads two relay-seeded events (waiting until
every row names the relay in its own `sources`, so the network really served
them), exports its session the way a shipped app persists one, and **quits
cleanly**. This is a third reason for the parent/child split, distinct from
C9's (`kill -9` proves nothing against an in-process `Engine` drop) and C17's
(#1796, process-wide measurement): a second `NMPEngine` built over the same
store path inside the process that just filled it is not a restart at all —
the Redb pages, the allocator and every decoded row are still in that address
space, so a read served from anywhere other than the durable file would look
identical. The scenario waits for the writer to be genuinely gone (exited,
waited on, `terminationStatus == 0`) before opening the store.

Then the relay is `SIGKILL`ed and required to REFUSE a real TCP connection —
before the restarted engine is built and again after every assertion, because
a relay that came back mid-scenario would otherwise be invisible. A fresh
engine over the same store path, still pointed at the now-dead relay URL, with
the persisted `NMPSessionPayload` restored, then proves in one run (~2.7s):

- the same account is signed in, with `providerKind == .localKey` and
  `signingAvailability == .available` — a login, not a public key. A restored
  account that cannot sign is a logged-in user who can do nothing, and a local
  key needs no network, so being offline is no excuse;
- exactly the two cached ids, two rows, **and their content** — ids alone
  would pass for rows that survived as bare keys;
- the query reports the dead relay honestly (`connecting`, never
  `requesting`/`finishedStoredEvents`/`coverageSatisfied`), so a stale feed is
  not readable as a complete one (bug-class ledger #7);
- `reconciledThrough` is still present across the restart. `SourceEvidence`'s
  own doc names this case — a source can be down while still carrying a
  perfectly good watermark from before it dropped — and it is what makes an
  offline cache reasonable-about rather than merely present. This is the
  public-API reading of #1087's claim.

Falsified three ways, each restored and re-confirmed green: a different, empty
store path returned `0` rows with empty content and a `nil` watermark; leaving
the relay UP left every feed assertion green (`rows=2`, right content) with
only the two reachability preconditions failing — the C2 analogue of C13's
fourth falsifier, and the same lesson; restoring with `sessionPayload: nil`
left `accounts=0`, `current=none`, `provider=nil`, `signing=nil`.

No API finding for C2 — `NMPSessionPayload`, `NMPSession.export()`, the
persistent `storePath` and the acquisition evidence all behaved exactly as
documented, with no app-side workaround anywhere in the scenario. One honest
observation rather than a finding: the cached feed comes back whether or not
the session is restored, because a literal-author filter needs no account.
The identity half and the feed half are independent claims and are separately
falsifiable, which is why both are asserted.

**C3 is proven live**, and it is this suite's first exercise of the same
event arriving from SEVERAL relays.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C3MultiRelayDedupProvenanceTests.swift`
runs three separate strfry processes with three independent LMDB stores, all
three in the engine's relay set. One event is seeded into relay A only; then
the IDENTICAL bytes -- same id, same signature, a real duplicate on the wire
rather than a re-signed lookalike -- are written into relay B, and later into
relay C, each over a real `EVENT` frame answered by a real `OK`. Throughout,
the app holds ONE canonical row whose `sources` grows
`[A]` -> `[A, B]` -> `[A, B, C]`. Passes in ~1.4s, stable over five runs.

**Three different failures are in scope, and the row count catches only one
of them.** Two rows is caught by the count and the id set. LOST PROVENANCE is
caught by requiring the exact three-relay set. A REPLACED row -- the row
removed and re-added rather than updated in place -- survives both, so C3
seeds a companion event alongside the shared one purely to give the delivered
array a second entry and therefore an INDEX that a remove-and-re-add would
disturb: an unbounded observation folds exact rebased deltas in arrival order,
so an in-place `sourcesGrew` leaves the position untouched while a
reinsertion moves the row to the end. The index is pinned before the first
growth and required to hold across both. Measured at index 1 on every run.
A fourth check exists because `sources` would otherwise be free to mean
nothing: the companion event never leaves relay A, and its provenance must
stay `[A]` -- an engine that unioned every relay it ever talked to into every
row would pass everything else.

**The precondition is the whole point.** "One row naming three relays" is
trivially true of an app handed the event by all three at cold start, with no
moment when the row had fewer sources than it finished with. So B and C start
EMPTY while being genuinely subscribed, and the scenario asserts before
seeding them: the row is delivered naming EXACTLY relay A; all three relays
report a wire subscription off `observeDiagnostics()`; and an ordinary client
REQ against B and C for that id comes back empty -- a fact about the fixture,
established over the relay's own wire protocol, never a reading of NMP state.

Falsified twice, each restored and re-confirmed green. **Seeding all three
relays up front is the one that matters**: `grewToB` and `grewToC` both stay
true, the final row is one row naming three relays, and every behaviour
assertion is green -- ONLY the three preconditions fail (`onlyRelayA=false`,
`relayBHasIt=true`, `relayCHasIt=true`). That is precisely the vacuous dedup
proof this scenario would otherwise have been, and it is the C13-fourth-
falsifier lesson repeating. Seeding relay C with a re-signed lookalike instead
(same author and content, `created_at + 1`, therefore a DIFFERENT id -- what a
lost-dedup engine effectively produces) failed on six assertions with the real
values: 3 rows against 2 distinct events, `maxRows=3`, the shared row's
`sources` stuck at two relays, and the pinned index moved from 1 to 0.

No API finding for C3 -- `Row.sources`, the `sourcesGrew` in-place update and
the per-relay diagnostics all behaved exactly as documented.

**C5 is proven live**, including the part most likely to have been broken.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C5ReplaceableDeletionStaleRedeliveryTests.swift`
drives three real relay processes and two concurrent observations, and proves
in ~4.7s: alice's kind:0 profile is superseded by a newer version and the old
one disappears (one row, v2's id, v2's content -- not two rows, not v1's text
under v2's id); a kind:5 carrying `["e", target]` removes exactly its target
and leaves the untargeted note alone; and a relay redelivering either the
superseded version or the deleted event AFTERWARDS resurrects neither.

**A correct engine's answer to a stale redelivery is to do nothing at all** --
no row change, no delivered batch, no app-visible fact -- which makes "the row
did not come back" equally true of an engine that ignored it and of a scenario
where the stale event never arrived. Three things rule that out. The two stale
relays are up, subscribed and empty from the start, so they are never a
late-connecting first delivery; an ordinary client REQ confirms each holds
nothing for the id before its phase. The stale event is written only after the
app has been ASSERTED to hold the post-supersede / post-deletion state. And a
CONTROL event the app has never seen is written to the SAME relay immediately
after the stale one: the app's subscription to that relay is one TCP
connection and the relay pushes in ingest order, so the control arriving is
positive proof the stale event was pushed first and silently refused. The
control's own `sources` is required to name that specific relay, which is what
identifies whose push path was proven live.

Falsified twice, each restored and re-confirmed green. **Writing the stale
events to the stale relays BEFORE the supersede and the deletion instead of
after** leaves `resurrected=[]` on both observations and every behaviour
assertion green -- only the two "already held it before this phase"
preconditions fail. Same shape as C3's and C13's, and the reason the
sequencing is asserted rather than assumed. Removing kind 5 from the app's own
filter fired four assertions at once with real values: the deletion never
removed its target (`deleted=false`, the feed still holding both notes), and
the stale relay then genuinely DID resurrect the target -- which is also an
independent confirmation that the no-resurrection assertion is live rather
than green by construction.

**C5's API finding, and it is a real one for app authors.** The feed's filter
has to include kind 5. Relays send only what an open subscription asked for,
so an app that subscribes to kind 1 alone is never sent the deletion and its
rows never go away -- NMP does not add kind:5 to an app's demand on the app's
behalf. Nothing in the read API hints at this, and the failure mode is silent:
deleted content simply stays on screen forever. The falsifier above is the
proof. A consequence worth stating plainly: because the app must subscribe to
kind 5, the deletion event itself is then delivered as an ordinary row in the
feed. That is consistent with NMP doing mechanics only and leaving display
policy app-owned, but it means every app doing deletions correctly also has to
filter kind:5 out of its own rendering.

**C6 is written and RED, and the red is a real finding (#1886), not a scenario
defect.**
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C6DeepWindowingTests.swift`
seeds 150 events one second apart over real `EVENT` frames, opens ONE
`.expandable(initial: 10, max: 100)` observation and scrolls it to the
ceiling, checking every page against a canonical `createdAt DESC, id ASC`
order the scenario computes from the events it signed itself -- never against
whatever NMP delivered. Runs in ~49s, of which 45 is the bounded wait that
establishes the finding.

Everything except the first advance PASSES: every page is the exact canonical
prefix (order and gaplessness in one assertion, reporting the missing and
unexpected ids and the first index that differs); every page EXTENDS its
predecessor rather than rewriting it, so a row that moves or repeats under the
app's scroll position fails even when the final set is right; no batch ever
carried a duplicate id; no batch ever carried more rows than the target in
force when it arrived; a request past `max` lands as a delivered
`.atBound(max: 100)` fact rather than a throw, with the row count unmoved; and
a live event arriving at an already-full window enters at the head and
DISPLACES the tail instead of making the window one row bigger.

**FAILING -- the first advance.** `requestRows(atLeast: 20)` on a window
opened at `initial: 10` delivers `WindowLoad.returned(added: 0)` and leaves
the window at 10 rows for a bounded 45s, with the relay up and holding all 150
events. It never self-heals, and re-issuing the SAME target is a documented
no-op, so an app has no way to ask again from where it is. Only raising the
target AGAIN moves it -- and that next raise then delivers its own value
exactly. In an app this is the first scroll-to-bottom of any windowed feed
doing nothing. Deterministic 5/5 across `(initial, firstTarget)` of (10,11),
(10,20), (10,50), (1,2) and (10,10)->(10,11), with 0ms/1s/3s settle beats
before the call, so it is neither a race nor a function of step size. The
assertion was NOT relaxed and the scenario was NOT reordered to warm the
window up first; C17's `distinct` phase is the standing precedent.

What the public API shows about the mechanism, printed on every run: the
query's own `SourceEvidence.status` for the relay walks `finishedStoredEvents`
-> `error` -> `awaitingRequest` -> `requesting` across that one advance, and
the relay's own log shows a NIP-77 negentropy session opened and closed inside
it. The engine-side root cause is in #1886.

**A correction C6 forced on its own preconditions.** The first draft asserted
that the relay reporting `finishedStoredEvents` meant the history was local,
and used that to justify the no-gaps assertions. It does not:
`SourceStatus.finishedStoredEvents`'s own doc calls it "a delivery fact about
ONE source answering ONE request", and a windowed observation's opening
request carries the window's `initial` as its wire limit, so only `initial`
events are local at that point. The precondition now claims only what it
proves -- that the opening request was answered, so the app is in the settled
first-page state a user scrolls from -- and every page assertion waits on the
delivered ROW COUNT rather than trusting any status to mean "everything is
here".

**C6's second finding: `WindowLoad.returned(added:)` is not a usable progress
signal.** Across repeated runs the SAME advance reported `.returned(added: 20)`
on some runs and `.returned(added: 0)` on others, with the 20 rows arriving in
a LATER batch carrying `.idle`. An app therefore cannot use the delivered load
fact to decide whether its scroll produced anything; the only reliable signal
is the delivered row count. C6's first draft asserted a nonzero
`returned(added:)` as its "the window really advanced" precondition and was
flaky because of it. The assertion was **removed rather than inverted** and the
value is printed on every run, so neither reading is silently promoted to a
contract -- the same treatment C13 gave `wireSubCount` during an outage.

Falsified twice besides, each restored: never writing history event #37 to the
relay -- a genuine hole in the middle, with row COUNTS still reachable so only
the order/gap check can see it -- failed at page 40 and every page after,
naming the missing id, the unexpected id and index 37 exactly. Opening the
observation with NO window failed the boundedness precondition with the real
captured value (150 rows delivered against the declared `initial` of 10) and
140 unexpected ids on the first page.

**C15's relay lab is qualified; C15 itself is NOT proven.** The distinction is
the whole point of the paragraph below, and an earlier revision of this section
got it wrong by calling C15 "proven live".

What the run below establishes is that **strfry can be driven through a
complete NIP-42 round trip on demand** — a prerequisite for the scenario, and a
real result, since the first two attempts to obtain one failed. What it does
not touch is NMP: the handshake is driven by the *controller*, not by
`NMPEngine`. So none of the following is yet proven — NMP notices the challenge,
NMP consults the configured AUTH policy, NMP signs a kind:22242 event bound to
that exact challenge and relay, or NMP recovers after a denial or a reconnect.

C15 becomes proven when the NMP path closes the round trip. Until then it
belongs under "not executed", not under the passing scenarios.

The route to the qualification corrected an earlier mistake worth keeping.

The first probe gated writes with a strfry `writePolicy` plugin that rejects
unless the connection is authenticated. That denial is real — but
`sendAuthChallenge()` is called from exactly three places in strfry's source,
and **the writePolicy rejection path is not one of them**. A plugin can reject
with a message that *says* `auth-required`, and no `["AUTH", challenge]` frame
is ever emitted. Two separate true facts — the plugin denies, and strfry sends
challenges — do not compose into "the plugin path triggers a challenge".

The fix was to use strfry's actual native trigger: mark the event NIP-70
protected (a `-` tag) rather than reach for a plugin. With the controller
driving the handshake itself — connect once, send EVENT, read AUTH+OK, sign a
kind:22242 event for that exact challenge and relay, send AUTH, resend the
original EVENT on the *same* connection — the round trip closes. Proven across
three runs, each with a different challenge nonce, which is also a second and
independent live confirmation of the freshness property previously only read
from source.

**Scenario-design consequence.** strfry has two genuine NIP-42 trigger
mechanisms — NIP-70 protected-event writes and `restrictedReadKinds` gated
reads, both challenge-driven — and a third, writePolicy-plugin blanket write
gating, which enforces access control for real but never goes through the
challenge machinery. A scenario wanting "every write here requires AUTH" must
mark its events `-`, or it gets a real rejection that is not a NIP-42 round
trip.

This also settles that the earlier failure was a client-tool limitation:
`nak`'s one-shot `--auth` cannot do it, because the handshake requires owning
the connection across all five steps.

## What is compiled, and what is not

The app **type-checks against the real NMP module**. Building the xcframework
with `scripts/build-swift-xcframework.sh --sim-only` produces
`ios-arm64_x86_64-simulator` and `macos-arm64` slices, after which
`xcodebuild` for the `Canary` scheme on an iPhone simulator reports
`BUILD SUCCEEDED` with zero errors, every `apps/Canary/Sources` file genuinely
`SwiftCompile`d against the real `NMP`/`NMPFFI` modules, and no warnings in any
of them. `build-for-testing` also succeeds, which additionally confirms the
test target's plain `import NMP` compiles.

This corrects commit `8575fa08`, whose message claimed `xcodebuild` "reaches the
link stage with no Swift compiler error" at a point when the xcframework did not
exist at all and the build failed while resolving the dependency graph — before
scheduling a single compile action. That claim was false in the direction that
flatters. A merged commit message cannot be amended, so the correction lives
here.

**What compiling does not prove.** Nothing here has been *run*. The
identity-override picker's behaviour, the Keychain round-trip, the
`UserDefaults` round-trip, and whether reattachment actually fires after a
real kill and relaunch all require a running app against the relay lab. Type-checking says the call sites match the real signatures; it says
nothing about whether the flows work.

The xcframework is a gitignored build artifact. A fresh checkout has no
simulator slice until that script is run, so "the Canary compiles" is a claim
about a machine that has built it, not about the repository.

## Findings the Canary has already produced

The point of the app is to surface places where the public API makes an
ordinary thing awkward. It produced one before it ever ran.

**Reads get outbox routing for free; writes do not.** A feed uses
`NMPDemand(source: .authorOutboxes)` with no configuration at all. But
`WriteRouting.auto` is a separate capability requiring `NMPConfig.outboxRouting`
to be populated with indexers, and its own documentation says an engine
constructed without them refuses it. An app that gets self-bootstrapping outbox
delivery on the read side gets nothing on the write side, and must discover a
second, differently-named capability to get the same routing philosophy for its
own notes.

An app author hits this and assumes they misconfigured something, because the
two halves of one idea do not look like one idea. Recorded as an ergonomics
finding rather than a defect: nothing is broken, and the asymmetry is real.

## What the Canary must not become

A generic testing framework, a huge demo application, an internal NMP
debugger, a mock relay, a protocol simulator, a synthetic benchmark detached
from app behaviour, a second implementation of NMP semantics in app code, a
dashboard project, or a new source of architecture policy.

If implementing or running it becomes more complex than understanding the NMP
behaviour it tests, simplify it.

## CI

The complete Canary is not a blocking CI job. A single scenario earns
promotion when it protects a named product guarantee, is deterministic in the
local lab, has caught a real regression or protects a historically expensive
bug class, **fails when the defect it protects against is deliberately
restored**, costs acceptably, and fails actionably rather than flakily.
Promote the smallest useful scenario, never the suite by default.

## Why Falsifier became the Canary

`apps/Falsifier` was 641 lines across nine files: a plain `@main App`, an
`@Observable AppModel` owning its own state, one `NMPEngine` built through the
public initializer, and five ordinary SwiftUI screens. Every source file
imported only `SwiftUI`, `Foundation`, `Observation` and `NMP`. An exhaustive
grep for `@testable`, testkit imports, internal-crate references, fake or mock
engines and stores, direct Redb access, and test-only constructors found
**exactly one hit in the whole tree**, in a test file rather than the app: a
vestigial `@testable import NMP` in a 23-line test that used nothing internal.

So Falsifier was already an ordinary application using only the public API.
Its gaps were missing *features*, not architectural contamination — which
makes it the Canary in embryo rather than something to discard. Keeping both
would have been two apps proving the same thing, which this document forbids.

`apps/UIGallery` is unaffected: a visual conformance and stress gallery for
the `NMPUI`/`NMPContent` rendering layer is a genuinely different
responsibility from "does an ordinary app work end to end against a real
relay".

## Related

- `apps/Canary/CanaryScenarios/README.md` — the `swift test` entry point,
  spelled out command by command, including both prerequisites.
- `docs/known-gaps.md` — unproven scenarios and unexplained failures stay
  visible there until resolved.
- `skills/nmp-dev/references/testing/` — the three-layer testing model the
  Canary is the third layer of. A green result at one layer never substitutes
  for a missing layer.
