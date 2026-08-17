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
rather than sleeps, two real-TCP probes — `isReachable`, a boolean for
"wait until this port stops answering", and `probe`, which reports the
actual errno (`.refused`/`.timedOut`/`.failed(errno:)`) and elapsed time,
because `isReachable` cannot tell a refused port from a black-holed one and
costs its full timeout for both (C8's finding, below) — plus an optional
shared `dataDir` so a second relay process can write into a stopped relay's
durable store on its own port — C13's and C14's outage window — and
`probeRead`, one plain NMP-free `REQ` on one fresh connection that reports what
the relay did about it (challenge, refusal text, events served), optionally
completing a NIP-42 handshake as a caller-supplied key first. That last one
exists so a scenario can prove a relay's demand INDEPENDENTLY of NMP: "NMP got
the row, therefore the relay demanded AUTH" is circular, and a relay that
challenged nobody would satisfy it. It knows nothing about NMP; it is a generic real-relay
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
convergence; C11 capability end to end (NIP-22 comments); C12 identity
freeze; C13 relay disconnect/reconnect; C14 NIP-77 reconciliation; C15
NIP-42 AUTH; C16 slow consumer and backpressure; C17 repeated lifecycle
churn; C18 clean shutdown.

**C11 was renamed** (#1875). It was listed as "semantic capability" — a phrase
that appeared exactly once in this entire repository, in the line above,
introduced by the commit that created the Canary. No definition anywhere, no
matching public API, no `semantic` surface in `crates/nmp/src/`, and no
protocol category by that name: a named category that does not exist, which
standing convention 3 forbids
(`docs/internals/conventions/naming-no-invented-categories.md`). The name now
comes from the public surface — "capability" is
`docs/internals/crate-architecture.md` rule 2's own word and `nmp-ffi`'s own
feature keys, "NIP-22 comments" is the protocol's name and the name of
`Packages/NMP/Sources/NMP/NIP22.swift`.

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
  and C12: there was no identity for a resumed write to remain frozen to. C9
  and C2 are proven through `NMPSessionPayload`. C12 turned out not to need a
  restart at all — the account switch it is about happens inside one live
  engine — and is proven above.

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

**C11 is proven live for the root shape that works, and its second half is a
real finding left red on purpose** (#1875).
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C11CommentCapabilityEndToEndTests.swift`
is the first exercise anywhere of a capability driven end to end against a
real relay. Every other test of a capability — `NIP22Tests.swift`,
`NIP22Test.kt`, the Rust crate tests — runs in-process with no relay, so
composition and decoding were covered and "the composed write reaches a relay
and comes back through an ordinary query" was not covered at all.

**NIP-22 rather than NIP-29**, and the reason is about the lab rather than the
SDK. NIP-29 has the larger Swift surface, but its group records
(kind:39000/39001/39002 metadata, admins, members) are RELAY-generated, and
strfry has no NIP-29 implementation: `createGroup` there publishes an event
nothing acts on and `observeRecords` would report `.unavailable` forever.
Proving NIP-29 end to end needs a NIP-29 relay in the lab — real work, and a
separate scenario. NIP-22 comments are ordinary events any relay stores, so
the capability rather than the relay's feature set is what is under test.

**PASSING — the capability composes into both ordinary nouns.** Two apps: two
`NMPEngine`s over two store paths, one relay, and no other channel between
them, so "the reader saw it" cannot be satisfied by a row that never left the
writer's own store. The author app composes a top-level comment with
`commentIntent(on: .root(...))` on a NIP-73 web root and publishes it through
the ordinary `NMPEngine.publish`; the reader app — which has never been told
the event exists — is delivered it through `commentThreadDemand(root:)` on the
ordinary `engine.observe`, replies with `commentIntent(on: .row(...))` off the
row NMP delivered it, and the author's own still-open thread query then holds
BOTH events. One demand covers the whole thread, exactly as documented. A
third query, a plain `NMPFilter(kinds: [1111], authors: ...)` that has never
heard of NIP-22, holds the same two rows — the capability's writes are
ordinary events in the ordinary store. `decodeComment` closes the round trip:
identical root at both depths, `parent == .root` for the top-level and
`.comment(eventID:authorPubkey:)` for the reply, whose parent the app never
stated. ~1.3-2.3s, stable over five runs.

**The preconditions are asserted before the behaviour, twice per write.** A
scenario that never actually sent anything reads green, so before any "the
thread came back" assertion runs, each write must have reached
`RelayState.published` for this exact relay on its own `receipt.status` AND
been handed back by the relay itself over an independent real `REQ` by id
(`RelayLabKit.queryById`, a plain socket with no NMP in it).

**Falsified three ways, each restored and re-confirmed green.** Pointing both
thread demands at a valid-but-different page left the write fully landed
(`publishedHere=true`) and delivered `0 rows [] over 4 batches` — so the
demand's `#I` scoping is doing real work rather than "any kind:1111 will do".
Routing the comment to `ws://127.0.0.1:1` failed at the precondition with
`publishedHere=false` before a single behaviour assertion ran. Inverting the
reply's parent to `.root` failed on the real captured
`comment(eventID: "a5ea1ba3...", authorPubkey: Optional("1544818d..."))`.

**FAILING — `commentThreadDemand` cannot read back two of its three root
shapes (#1876), left red on purpose.** The demand binds the root identifier to
the `#I` tag for EVERY root shape, but the composer writes `E` for an event
root and `A` for an address root — `I` only for a NIP-73 external one. So the
app-shaped case, commenting on a note, composes and publishes perfectly and
can then never be read back through the capability's own door. The scenario's
second test proves it is the demand and nothing else: same comment, same
relay, same run, provably on the relay by real `REQ`, delivered to a
hand-built `NMPFilter(kinds: [1111], tags: ["E": ...])`
(`1 rows`) and not to `commentThreadDemand(root: .event(...))` (`0 rows [] over
6 batches`, selection tags `I=literal(["e7a36fe4..."])`). The assertion is
written the right way round — it goes green with no edit when #1876 lands —
rather than inverted into a claim that the current behaviour is correct, which
would report the fix as a regression. Same discipline as C17's `distinct`
phase.

That workaround is itself the finding's shape: it requires the app to know
NIP-22's uppercase-root tag vocabulary, which is exactly the knowledge the
capability crate exists to own.

**C11's second API finding (#1878): a NIP-73 web root does not survive its own
round trip.** Composed as `Nip73.url(url:)`, it decodes back as
`Nip73.general(value:kind: "web")` — deliberately, since the decoder never
re-canonicalises a read and `.url`'s meaning is "already canonical". The two
values carry the identical string, render identical `I`/`K` rows and produce
the identical demand, but they are different cases of a `Hashable` enum, so
`decoded.root == theRootIComposed` is `false` for every web thread and an app
keying comments by their root splits one page's thread in two. Swift exposes
no `iValue`/`kValue` accessor and no canonicalising constructor, so the only
public way to ask "same thread" is to build `commentThreadDemand` from each
and compare the `NMPDemand`s — which is what the scenario asserts, printing
both values on every run so neither shape is silently promoted to a contract.
The assertion was written on the demand rather than inverted onto the value,
the same way C13 handled `wireSubCount`.

**One ergonomics observation, not a defect.** `commentIntent` returns
`WriteRouting.auto`, and `.auto` needs `NMPConfig.outboxRouting` indexers a
lab engine does not have, so the app assigns `intent.routing =
.explicit(relays:)` on the returned value — a plain public field on the
ordinary write noun, exactly what C7 does. This is the same read/write routing
asymmetry already recorded below, seen from the capability side.

**C8 is proven live** (#1880), and it is the first publish in this suite
into a relay that is *down* rather than *hung*.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C8PublishWhileRelaysFailTests.swift`
publishes one real signed event to three real strfry destinations, two live
and one whose process was started and then `SIGKILL`ed, in ~1.5s. The
distinction from C9 case 3 is the whole point: that scenario's unreachable
relay is `SIGSTOP`ed, so its listening socket stays open and the app's
packets are accepted and never answered — a hung relay, which fails by
timeout. C8's fails by `ECONNREFUSED` on the first syscall, and nothing here
had ever published into that.

Both healthy relays reach `.published` while the dead one is still
`waiting(notConnected)`, and each of the three claims is asserted
separately: the `destinations` fact names all three with `complete == true`;
the receipt reports a fact of its own for the dead relay rather than falling
silent about it; the durable `publishQueue` entry lists all three relays
with a per-relay state for each; both live relays serve the event back over
their OWN wire (relay-side truth, independent of anything NMP claims); and
the single canonical row's provenance grows to exactly the two relays that
echoed it, never the third.

**The precondition needed a new instrument, and finding that out is a
RelayLabKit finding.** The first draft asserted the dead relay was
unreachable *and fast about it* through `RelayHandle.isReachable`, and failed
on its first run with the relay genuinely dead: `isReachable(timeout: 2)`
returned `false` after **2.0057 seconds** — the entire budget.
`Network.framework` classifies a refused connection as
`NWConnection.State.waiting(.posix(ECONNREFUSED))` and keeps retrying it, so
a probe watching for `.failed` can only ever end on its own timeout, and a
refused port is indistinguishable from a black-holed one through that door.
`isReachable` remains correct for "wait until this stops answering", which is
all C2 and C13 ask of it. C8 needed the errno, so `RelayLabKit` gained
`RelayHandle.probe` — a non-blocking `connect(2)`/`poll(2)`/`SO_ERROR` probe
reporting `.accepted`/`.refused`/`.failed(errno:)`/`.timedOut` with elapsed
time. Measured against the same dead port: **`refused` in 0.0001s**.

**Falsified two ways, each restored and re-confirmed green.** Not killing the
third relay at all left every behavioural assertion green and failed on the
refusal precondition (`accepted` vs `refused`), on the down relay's real
history `["waiting(notConnected)", "waiting(needsAuth)", "sent(attempt=1)",
"published"]`, and on the end-of-scenario re-probe. Routing the write to only
the two healthy relays — the app quietly dropping the failed destination —
failed on three independent assertions with real values: `destinations` named
two rather than three, the receipt reported `(never reported)` for the third
across 11 facts, and the queue entry listed `[]`.

**What C8 deliberately does not assert.** The write never reaches
`WriteOutcome.settled`, and that is correct rather than a defect: settlement
needs every destination terminal, the only terminal a permanently-unreachable
relay could reach is `.gaveUp`, and offline time deliberately consumes no
attempt ordinal — so a relay that is simply down never spends the ceiling.
`outcome=nil` is printed on every run.

**C10 is proven live** (#1880-adjacent; see the issue list), the write-side
half of the local-first claim C2 proved for reads.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C10OfflineWriteThenConvergenceTests.swift`
publishes one note with **nothing reachable at all**, then restarts the relay
on the same port and requires the write to go out and settle by itself. It
does, in a measured **7.7-9.9s** after the relay returns (~7-12s per run), on
the SAME `Receipt` the single `publish` call returned: no re-publish, no
`reattachReceipt`, no second engine, no reopened query, no app-side retry
anywhere in the file.

Proven along the way: the row is visible through the app's own live query
while offline with `sources` empty; the local key signs with no network; the
relay serves the event back over its own wire afterwards with the right
pubkey and content; the row's provenance grows to include the relay with the
row count still 1; and the relay holds **exactly one** event by this author.

**Two preconditions, and the falsification proves they are the only thing
standing between this scenario and vacuity.** Leaving the relay UP for the
write and taking it away only afterwards left *every single convergence
assertion green* — `converged=true in 0.3s`, the relay serves it back,
provenance grew, one row, one id — and was caught ONLY by the two
preconditions: the port probed `accepted` at write time, and a second strfry
process over the SAME LMDB directory on its own ephemeral port
(`RelayHandle(dataDir:)`, C13's mechanism used to prove ABSENCE) found the
event already in the relay's durable store while its port was dead. That is
C13's fourth falsifier in write form, and it is now mechanically ruled out.
The second falsifier — never restarting the relay — failed red with the real
stuck values: `waiting(notConnected)` for the full 120s, `outcome=nil`.

**What C10's duplicate check can and cannot prove.** The relay is asked for
every event this author wrote and must hold exactly one, which rules out the
failure that actually loses data: a write re-signed on the retry path is a
DIFFERENT event id, since an id is the hash of its contents. It does NOT rule
out NMP sending the identical EVENT frame twice — the relay deduplicates by
id and the receipt looks the same. Same limit C9 recorded for its no-resend
assertion; closing it needs relay-side inbound frame counts or a public
delivery-attempt fact, and neither exists.

**C12 is proven live, and it was the last scenario this document listed as
blocked.** The blocker was recorded as "session identity did not survive
restart… there was no identity for a resumed write to remain frozen to". It
turns out C12 needs no restart at all: the switch it cares about happens
inside one live engine, which is exactly what an app does when a user taps a
different account.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C12IdentityFreezeTests.swift`
covers the two moments a write can be frozen at, which fail differently:

1. **Unsigned and parked** (~5.8s). Alice signs in with a PUBLIC KEY ONLY —
   an ordinary app state, a user whose signer is not attached yet — and
   publishes. The write parks at `awaitingSigner(alice)`. Bob is then added
   WITH a private key and made current, and the scenario waits, deliberately,
   giving a re-resolving engine every chance to sign alice's parked write
   with bob's available key. It does not: the signing stage names alice and
   only alice across the write's whole life. Alice's private key is then
   added with `makeCurrent: false`, and her write signs, publishes and
   settles **while bob is the current account**. Measured signing history:
   `["awaitingSigner(alice)", "inFlight(alice)", "signed(...)"]`. This is the
   case where re-resolution would be invisible — the wrong event would be
   perfectly valid and perfectly signed.
2. **Signed and undelivered** (~8-10s). Alice signs, her only relay is a
   refused port, bob becomes current, the relay returns, and the delivered
   event still carries alice's key.

Both cases end at the RELAY, over its own wire: alice must hold exactly the
one event and **bob must hold nothing at all**. A re-signed write shows up
there as an event by the wrong author, and no amount of correct internal
bookkeeping hides it. `PublishQueueEntry.pubkey` reads alice before and after
the switch in both cases.

**The mid-flight precondition is asserted, and falsification shows why.**
Giving alice her signer FIRST — so the write signs, publishes and settles
under her, and only then switching to bob — left every identity assertion
green (alice has 1 event, bob has 0, stored pubkey is alice's) and was caught
only by `PublishQueueEntry.outcome` reading `settled` at the instant of the
switch. Not switching at all in case 2 failed on the switch precondition with
the real key. Inverting case 1's relay-side identity claim (assert the event
belongs to bob) failed showing the real values in both directions.

**Three findings from C8/C10/C12, all small, all real.**

`RelayWaiting.needsAuth` appears against a strfry with no NIP-42
configuration that never sends an `AUTH` frame. Every write in C8 and C10
walks `notConnected → needsAuth → sent → published`. Nothing is broken — the
write lane needs its own identity-scoped session established before it can
send — but the sentence an app builds out of that case is "this relay wants
you to authenticate", which is not what happened. Recorded, not asserted.

`publishQueue(forEventID:)` returns **zero entries once a write settles**
(measured during C8's falsification, where all three relays succeeded). The
queue is outstanding obligations, exactly as its doc says, not a history of
writes — so an app that wants to know where a FINISHED write went must have
been holding its receipt. Worth stating because "read your own publish queue
back" reads like a durable record of writes, and it is a record of unfinished
ones.

`RelayHandle.isReachable` cannot distinguish a refused port from a
black-holed one and costs its full timeout for both (the `NWConnection`
finding above). That is a Canary-lab defect rather than an NMP one, and it is
fixed by `RelayHandle.probe` rather than by changing `isReachable`, whose
boolean is still the right tool for the waits C2 and C13 use it for.

**C14 is proven live** (#1888), and the thing it proves is not "the events
arrived" — that is equally true of a refetch — but that NMP **transferred the
difference instead of the whole set**.
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C14Nip77ReconciliationTests.swift`
holds ONE feed open across a real strfry outage during which the relay's LMDB
store gains ten events the local Redb store does not have. The two stores are
named plainly: NMP's own durable store is one, the relay's is the other. There
is no app-visible "reconcile these peers" call anywhere in the public API, so
the scenario is written the way an app actually meets NIP-77 — it doesn't.

The divergence is deterministic by construction, not a race won: the ten
outage-window events are written by a SECOND strfry process on its own
ephemeral port over the SAME LMDB directory (C13's `RelayHandle(dataDir:)`)
while the port the app is dialing provably refuses a real TCP connection, and
the scenario checks that the app is holding exactly the sixty overlap events at
that moment.

**The oracle is `RelayDiagnostics.eventsByKind`** — "events actually RECEIVED
from a relay, counted by kind" — sampled before the outage and again after
convergence. Measured, with `relay.negentropy.enabled = true`: 60 → 70, a
delta of **10** for a 10-event difference, with `nip77Handoff` walking
`none → awaiting_live_eose → backfilling → live` and `nip77Behavior` reaching
`behaviorally_proven`. The identical flow against the identical relay with
`relay.negentropy.enabled = false` also converges on all 70 rows — over a
delta of **70**, 60 → 130, with `nip77Advertisement` at
`advertised_unsupported`, `nip77Behavior` never leaving `unknown`, and
`nip77Handoff` never leaving `none`. Both runs are committed; neither half is
asserted alone, because "converged" without the count proves nothing about
NIP-77 and the count without the negentropy-off control is not evidence that
the count is caused by NIP-77.

**A real finding, found by writing the scenario, and the reason it has the
shape it has.** The first draft primed the store, shut the engine down, seeded
the divergence and restarted — the obvious shape. It converged **while
receiving all 70 events**, with NMP simultaneously reporting
`nip77Behavior = behaviorally_proven` and `nip77Handoff = none`. The NIP-77
capability probe is asynchronous: a fresh engine places its query's REQ as soon
as the socket is up, and `begin_neg_handoff` is only reachable when
`prober.probed(&relay)` already holds a verdict at the moment a request is
placed. The verdict lands afterwards, and nothing re-plans the in-flight
request. **A cold start therefore never reconciles — it always refetches, and
learns the relay supports NIP-77 just too late to use it.** Reconciliation is
reachable on a LATER request: a reconnect replay, or a new filter. So the
committed scenario establishes the probe verdict first and asserts it as a
precondition, which is exactly what makes the measurement afterwards a
measurement of reconciliation being used rather than of it still being
discovered.

Falsified three ways, each restored and re-confirmed green. Seeding the
divergence BEFORE the outage instead of during it left convergence trivially
true and was caught only by the divergence precondition ("the app already held
10 outage-window events before the outage ended") — C13's fourth falsifier,
same shape, same lesson. Inverting the efficiency assertion failed on the real
captured `10` against a deliberately wrong `60`. Pointing the negentropy
scenario at the negentropy-disabled relay failed on all four NIP-77 assertions
at once with real values: `advertised_unsupported`, `unknown`, handoff
`["none"]`, and 70 events transferred.

**C14's API finding.** Reconciliation is invisible to the app that benefits
from it. There is no `sync()`/`reconcile()` call, no demand option, and — more
consequentially — no PER-QUERY fact distinguishing "this coverage came from a
completed negentropy round" from "this came from a plain REQ": NIP-77 coverage
is attributed through the exact same `attribute_eose` path as EOSE, so
`SourceEvidence.reconciledThrough` and `SourceStatus` are identical either way.
The only public distinguisher is the per-RELAY, engine-global
`RelayDiagnostics.nip77Advertisement`/`nip77Behavior`/`nip77Handoff` triple,
and `nip77Handoff` is a transient — a snapshot read after reconciliation
finished says `live` and nothing about how it got there, so an app must hold
`observeDiagnostics()` open and accumulate. This scenario does exactly that,
and would have measured nothing without it.

**C15's relay lab is qualified; C15 itself is NOT proven** — and it is now
proven not to work, which is a stronger and more useful result than "not
executed". The distinction between lab and subject is the whole point of the
paragraphs below, and an earlier revision of this section got it wrong by
calling C15 "proven live".

**C15 is committed and RED, on purpose** (#1887), with the failure diagnosed:
**NMP's NIP-42 deadlocks against any relay that challenges in response to a
request, which is to say against strfry (#1889).**
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/C15NIP42AuthTests.swift`
uses strfry's real native read trigger, `relay.auth.restrictedReadKinds`, and
leaves `restrictReadToInvolvedPubkey` on, so the seeded events are `p`-tagged
to NMP's own account and the relay serves them ONLY to a connection
authenticated as exactly that key. "The row arrived" would therefore have meant
"NMP authenticated as its own account", not merely "some AUTH happened".

**Every precondition passes, and each is proven by an NMP-free client**
(`RelayHandle.probeRead`, new in `RelayLabKit`): an unauthenticated plain
client issuing the identical filter receives a real `["AUTH", <challenge>]`
frame plus `CLOSED ... auth-required: requested filter requires
authentication` and zero events; a client that authenticates as a DIFFERENT key
completes the handshake (`successfully authenticated`) and is still served zero
events; a client that authenticates as an involved pubkey is served the row. So
the relay demands AUTH, the challenge is real, the identity scoping is live,
and the data is retrievable.

**Then NMP does nothing.** It opens the `.nip42` session, reports it under
exactly the requested access identity, reaches
`awaitingAuth(phase: .awaitingChallenge)` and stays there indefinitely. It
transmits nothing at all — not the AUTH proof, not even the REQ. The installed
`NMPAuthPolicy` is never consulted. `AuthDiagnostics` shows
`policyBound=false`, `signerBound=false`, `challengeDescriptor=nil`,
`transportGeneration` incrementing across reconnects and nothing else moving.

**The scenario pins the cause through the public API alone**, with a control
phase: the SAME relay and SAME filter observed under `.public` instead of
`.nip42`. One field different, two outcomes —

```
CONTROL (.public):  status=error                           history=["connecting", "error"]
SUBJECT (.nip42):   status=awaitingAuth(awaitingChallenge)  history=["connecting", "awaitingAuth(awaitingChallenge)"]
```

The `.public` session's REQ reaches the relay and is answered (with the
auth-required refusal, hence `error`). The protected session is the one that
never transmits. Recorded, not asserted — a diagnosis printed alongside a
failure, never promoted to a contract, the same treatment C13 gave
`wireSubCount`.

The mechanism, confirmed in the engine: NMP drops a protected session's ops
from the wire delta until that session has completed AUTH
(`crates/nmp-engine/src/core/query.rs`), replays planned REQs at connect only
for a `Public` session (`core/auth_transport.rs`), and constructs an
`AuthSessionState` in exactly one place — on an INBOUND `["AUTH", challenge]`
frame. strfry only emits that frame in response to a request it wants to gate;
`canary.md` already records both of its genuine triggers as challenge-driven.
So NMP waits for a challenge the relay will only send once NMP sends the
request NMP is withholding until it is challenged. Neither side is wrong
alone; together they never exchange a byte.

**Why no existing test caught this.** Every green NIP-42 test uses a relay
shape that challenges unsolicited, or fabricates the challenge outright:
`auth_core_headless.rs` injects a synthetic `RelayMessage::Auth` directly into
the reducer; `nmp-parity`'s scenarios use `ScriptedRelay`'s
`auth_required_writes`, whose own doc says it does not challenge on connect and
does not gate reads, so no protected-READ demand is exercised anywhere; and
`crates/nmp/tests/integration_capstone.rs` had to hand-build a relay that
sends AUTH "immediately after the WebSocket handshake, before the client has a
chance to send a REQ or EVENT". That bespoke relay exists precisely because the
standard fixture does not challenge unsolicited — which means the only relay
shape the current engine can complete a NIP-42 round trip against is one built
for it. This is the exact gap the Canary exists to find, found the first time
an ordinary application drove the surface against a real third-party relay.

**Left red rather than reshaped**, on the principle C17's #1846 phase
established: the scenario is not weakened until something passes. It is written
as the scenario that will be green when #1889 closes — the round trip, the
denial surfacing as `authDenied`, recovery on a fresh session, and re-AUTH
after a reconnect are all already asserted and all already have their
preconditions — so closing the gap is what turns it green.

Falsified twice, each restored and the red state re-confirmed. Because the
scenario's *result* is a failure, what has to be falsifiable is its
*preconditions*: pointing `restrictedReadKinds` at a kind the scenario does not
use made the unauthenticated probe receive no challenge and be served the row
outright, failing three preconditions with the real captured frames (and,
usefully, turning the `.public` control green at `finishedStoredEvents`, which
confirms the auth gate is the only thing holding it at `error`). Turning
`restrictReadToInvolvedPubkey` off made the wrong-identity probe be served the
row, failing exactly the one identity-scoping precondition and nothing else.

**The lab qualification that came first**, and still stands: **strfry can be
driven through a complete NIP-42 round trip on demand** — a prerequisite for
the scenario, and a real result, since the first two attempts to obtain one
failed. What it does not touch is NMP, since the handshake is driven by the
*controller*. C15's own scenario above is what drives `NMPEngine`, and the
`RestrictedReadProbe` it uses for its preconditions is the read-side successor
to this write-side qualification.

The route to that qualification corrected an earlier mistake worth keeping.

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
