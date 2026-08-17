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
rather than sleeps. It knows nothing about NMP; it is a generic real-relay
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
signed event with no correlation token at all, prints machine-readable
stdout markers as it reaches each fact the parent needs, then parks doing
nothing further. The parent waits for a marker with a bounded timeout,
`kill -9`s the child for real, then opens a FRESH engine in-process over the
SAME store path with the persisted session restored, finds the obligation by
enumerating `publishQueue` — not by a correlation token remembered across
the crash, which would just be an app-owned shadow ledger of NMP's own
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
`NMPSessionPayload`, `WriteIntent`'s `correlation`, and `reattachReceipt(correlation:)`
exactly as documented. The one thing worth naming precisely: a restarted
app that lost the account's session payload cannot resume a still-unsigned
write at all (there is no signer), which is why persisting the session
payload is not optional plumbing but a hard prerequisite for any of this to
work — exactly the case the shipped `AppModel`/Compose session persistence
already covers, but worth stating as a discovered fact here rather than an
assumption.

**Superseded (#1770):** the paragraphs above describe C9 as it was first
proven, when the app minted its own correlation token and the parent test
handed that same token to the recovery half by argv. That shape proved the
engine recovers a *known* obligation, not that an app can *find* one after a
crash — the actual half an app author has to build. `publishQueue` already
answered "what have I got outstanding" without an app-side ledger
(`Receipt.swift:184`), so both the shipped `AppModel` and this test's
recovery half now enumerate it instead: no correlation token is minted by
`canary-c9-publisher` at all, and `assertRecovery` finds the obligation via
`publishQueue(limit:)` before reattaching by `reattachReceipt(id:)`. The
Canary's own `UserDefaults` correlation ledger (`AppModel`/`ComposeView`,
built twelve days after `publishQueue` shipped) is deleted for the same
reason.

Two facts about the starting position, established by survey:

- **The app has never called `publish`.** Six scenarios (C5, C7, C8, C9, C10,
  C12) have no product surface at all — not because the SDK lacks anything
  (`WriteIntent`, `Receipt`, the correlation token and `reattachReceipt` are
  complete and well specified) but because no screen uses them.
- **Session identity did not survive restart**, which silently blocked C2, C9
  and C12: there was no identity for a resumed write to remain frozen to.

**C15 is proven live**, and the route to proving it corrected an earlier
mistake worth keeping.

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
pending-correlation `UserDefaults` round-trip, and whether reattachment actually
fires after a real kill and relaunch all require a running app against the relay
lab. Type-checking says the call sites match the real signatures; it says
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
