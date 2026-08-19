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
  (`Packages/NMP/README.md`).
- `strfry` must already be built: `apps/Canary/setup-strfry.sh`. Each
  scenario locates it at `$RELAY_LAB_CACHE_DIR` (default
  `~/Library/Caches/nmp-canary-relay-lab`) itself and calls `XCTSkip` by
  name if it is missing — proven by pointing `RELAY_LAB_CACHE_DIR` at a
  nonexistent path and confirming both scenarios report `Test skipped -
  strfry is not built at <path> -- run apps/Canary/setup-strfry.sh first`
  rather than crashing or hanging.

Once both exist, `swift test` from `apps/Canary/CanaryScenarios` runs the
suite. It is cheap enough to run after every change.

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

## Scenario findings

C1 cold start and live feed; C2 cache then offline restart; C3 multi-relay
dedup and provenance; C4 reactive derived query; C5 replaceable, deletion and
stale redelivery; C6 deep windowing; C7 normal publish; C8 publish while
relays fail; C9 crash/restart during publication; C10 offline write then
convergence; C11 capability end to end (NIP-22 comments); C12 identity
freeze; C13 relay disconnect/reconnect; C14 NIP-77 reconciliation; C15
NIP-42 AUTH; C16 slow consumer and backpressure; C17 repeated lifecycle
churn; C18 clean shutdown.

C11 was renamed (#1875). It was listed as "semantic capability" — a phrase
that appeared exactly once in this entire repository, introduced by the commit
that created the Canary. No definition anywhere, no matching public API, no
`semantic` surface in `crates/nmp/src/`, and no protocol category by that
name. The name now comes from the public surface: "capability" is `nmp-ffi`'s
own feature-key word, "NIP-22 comments" is the protocol's name and the name of
`Packages/NMP/Sources/NMP/NIP22.swift`.

Each scenario `CN` has one test file,
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/CN*Tests.swift`. All
eighteen exist. **The numbers live in those files, not here.** Each test's
header states its corpus size, cadence, cycle counts and committed bounds, and
several state what their bound cannot resolve — C17's file, for instance,
records that its 128 B/cycle heap bound is roughly three times the measured
noise floor and cannot see a smaller leak. Timings, byte counts and run counts
were deliberately removed from the prose below: repeated in a document they
drift away from the assertion, lose the caveat that made them honest, and read
as evidence when they are a recollection. Run the test.

### C1 — cold start and live feed: PROVEN LIVE

Seeds an event before the engine exists, opens one observation, then seeds a
second event inside that same still-open subscription and proves it arrives
live with no duplicate canonical row. Sabotaged with a wrong-but-valid pubkey
filter to confirm it fails red, then restored.

### C2 — cache then offline restart: PROVEN LIVE (#1864)

A warmer process reads relay-seeded events, exports its session and quits; the
relay is then killed. A fresh engine restores the session, serves the cached
rows, reports the dead relay honestly, and keeps its `reconciledThrough`
watermark across restart.

### C3 — multi-relay dedup and provenance: PROVEN LIVE

The same event bytes written to three separate strfry processes produce one
canonical row whose `sources` grows as each relay echoes, with its list index
pinned. A re-signed lookalike is correctly treated as a distinct event.

### C4 — reactive derived query: PROVEN LIVE (#1871)

The first `NMPBinding.derived` exercise: a kind:1 feed of authors projected
from a kind:3 contact list's `p` tags. Publishing a replacement kind:3 makes
the feed deliver the new author's notes with zero app action. Whether the
binding retracts rows on an unfollow is untested.

### C5 — replaceable, deletion and stale redelivery: PROVEN LIVE

A superseded kind:0 leaves one row; an `["e", target]` kind:5 removes exactly
its target; stale relays redelivering superseded and deleted events after the
app holds the post-state resurrect neither. API finding: the app must subscribe
kind 5 itself — NMP will not add it — and must then filter kind:5 rows out of
its own display.

### C6 — deep windowing: first advance FIXED (#1886), one open defect

Every window advance but the first behaves correctly: exact prefix, extends
rather than rewrites, no duplicates, the `.atBound` fact, live arrival
displacing the tail. The first `requestRows(atLeast:)` from the initial window
does not advance, deterministically, across every pair the test tries.
`.returned(added:)` is separately unreliable — the same advance reports
different counts across runs — so that assertion was removed rather than
inverted. Filed #1886.

### C7 — normal publish: PROVEN LIVE

A real signed event via `.explicit(relays:)`. The row is visible locally before
the relay's confirmation reaches the receipt stream; `sources` then grows on
that same row rather than producing a second; the row query and the receipt
stream agree on the event id. Falsified three ways: a dead address, an inverted
acceptance-visibility assertion, and an inverted echo/dedup assertion.

### C8 — publish while relays fail: PROVEN LIVE (#1880)

Three destinations, one of them killed. Healthy relays reach `.published` while
the dead one stays `waiting(notConnected)`; `destinations`, the receipt and
`publishQueue` all name all three; provenance grows to exactly the relays that
echoed. Lab finding: `isReachable` burns its whole timeout against a refused
port, which is why `RelayHandle.probe` was added.

### C9 — crash/restart during publication: PROVEN LIVE

A real `kill -9` of a sibling publisher process at three kill points, with
recovery through `publishQueue` enumeration and `reattachReceipt(id:)` only.
Recovers after local acceptance before delivery; recovers once a
never-reachable relay is healed after the crash; and shows no regression from
`.published` when one of two relays is partitioned. That last case cannot rule
out a harmless resend, because the oracle is `receipt.status` only.
Superseded by #1770: correlation-token recovery, `WriteIntent.correlation`,
`CorrelationToken` and `reattachByCorrelation` were all deleted.

### C10 — offline write then convergence: PROVEN LIVE

A publish with nothing reachable settles itself on the same `Receipt` once the
relay returns — no re-publish, no reattach, no second engine. The offline row
is visible with `sources` empty, and the local key signs with no network. As in
C9, an identical resent frame cannot be ruled out, since the relay deduplicates
by id.

### C11 — NIP-22 comment capability: PARTIAL, root-shape read-back FAILING (#1876)

The first capability test against a real relay. A comment on a NIP-73 web root
publishes and reads back. FAILING for other root shapes: the demand binds the
root to `#I` for every shape while the composer writes `E`/`A` for event and
address roots, so ordinary-note comments publish but never read back (#1876). A
second API finding (#1878): `Nip73.url(url:)` decodes back into a different
`Hashable` case carrying the same string, so a composed root never compares
equal to its decoded self.

### C12 — identity freeze: PROVEN LIVE

Two moments. An unsigned write parked under one account signs under that
account once its key is added, even though a different account became current
in between. A signed but undelivered write still carries its original author's
key when its relay returns. Both proven at the relay: one account holds the
event, the other holds none.

### C13 — relay disconnect/reconnect: PROVEN LIVE (#1863)

The first exercise of two observations sharing one query — the shape that
missed #1848 elsewhere. Six phases: both delivered over one wire subscription;
the port killed and both reaching `disconnected`; an event written while the
port is provably dead; both resuming with zero app action; closing one sharer
leaving the other receiving; closing the last dropping the wire subscription.
API finding: `wireSubCount` does not fall during an outage — it counts planned,
not live, subscriptions (#755).

### C14 — NIP-77 reconciliation: PROVEN LIVE (#1888)

A feed held open across an outage in which the relay gains events the local
store lacks. With negentropy on, convergence costs a small delta; with it off,
the control arm converges by refetching the whole set. Finding: a COLD-START
reconnect refetches everything anyway — the probe verdict lands too late for
the in-flight request, so reconciliation only helps a later one. API finding:
no per-query fact distinguishes negentropy coverage from a plain EOSE; only the
transient, engine-global `nip77Handoff` does.

### C15 — NIP-42 AUTH: PROVEN LIVE, found and closed #1889

Against a strfry configured with `restrictedReadKinds` and
`restrictReadToInvolvedPubkey`. NMP-free probes establish the preconditions,
then NMP walks the full AUTH state machine, survives a relay restart with a
fresh challenge, surfaces denial as `authDenied`, and recovers on a later
`.allow` without rebuilding. Found #1889, a real deadlock, committed red
first: NMP withheld a protected session's REQs until AUTH but only built its
auth state off an inbound challenge strfry never sends unsolicited. The fix
was to send REQs regardless of identity binding — three conditionals removed,
none added.

### C16 — slow consumer and backpressure: PROVEN LIVE (#1869)

A flood against a deliberately slow reader. Both arms end holding every id with
no duplicates and no loss. The scenario's own finding is that currency is the
wrong axis: because every delivery is a full snapshot, a reader pulling far
less often still holds nearly everything at flood end, so the committed
precondition uses read count plus deepest backlog instead. Falsified by a
zero-delay arm and by dropping rows in the reader. The iterator throttles
regardless of arm (#17).

### C17 — repeated lifecycle churn: PARTIAL, `distinct` phase FAILING (#1846)

Three phases against a sibling churner process (#1796). PASSING: repeatedly
opening and closing the identical filter leaks no file descriptors, threads or
heap; repeatedly constructing and shutting down engines returns to a flat
post-shutdown baseline. FAILING: opening a DISTINCT filter each cycle grows the
heap linearly with no plateau, measured at two cycle counts and confirmed at a
third. Filed #1846 and left red. Falsified by deliberately leaking a file
descriptor per cycle, and by leaking every query. API findings:
`observeDiagnostics()` has no synchronous snapshot read, and
`DiagnosticsSnapshot` exposes nothing about the #1846 retention.

### C18 — clean shutdown: PROVEN LIVE (#1870)

Outside-process preconditions first: `lsof` shows the child's live TCP
connections, and a second engine over its store path throws
`NMPError.storeAlreadyOpen` while it lives and succeeds after it exits. File
descriptors and threads return to the app's own pre-engine baseline after
release — including in the arm that never calls `shutdown()`. Falsified by a
deliberately leaked engine, which an earlier weaker assertion (comparing only
against the live peak) let pass; the assertion now compares against the
baseline. A hung teardown is also exercised.

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
`NMPDemand(source: .auto)` with no configuration at all. But
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
