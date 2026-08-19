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

Test files below live under
`apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/`.

### C1 — cold start and live feed: PROVEN LIVE

`C1ColdStartLiveFeedTests.swift`. Seeds one event pre-engine, opens
`.observe(.single(...))`, waits for the historical row, then seeds a second
event live in the SAME subscription with no duplicate row
(`Set(rows.map(\.id)).count == rows.count == 2`). Passed ~1s. Sabotaged
(wrong-but-valid 64-hex pubkey filter): failed red with the expected timeout
message; restored, re-confirmed green.

### C2 — cache then offline restart: PROVEN LIVE (#1864)

`C2CacheThenOfflineRestartTests.swift`. Sibling `canary-c2-warmer` reads 2
relay-seeded events, exports session, quits cleanly. Relay `SIGKILL`ed,
refuses TCP; fresh engine restores `NMPSessionPayload` and proves in ~2.7s:
`providerKind == .localKey` / `signingAvailability == .available`; exactly 2
cached ids, rows and contents; dead relay honestly `connecting`;
`reconciledThrough` watermark survives restart. Falsified: empty store path →
0 rows, nil watermark. Relay left up → feed green, only the 2 reachability
preconditions fail. `sessionPayload: nil` → `accounts=0 / current=none /
provider=nil / signing=nil`. No API finding.

### C3 — multi-relay dedup and provenance: PROVEN LIVE

`C3MultiRelayDedupProvenanceTests.swift`. Same event bytes written to relay A,
then B, then C (3 strfry processes); one canonical row, `sources` grows
`[A] → [A,B] → [A,B,C]`, index pinned at 1 every run (a companion event rules
out remove-then-readd). Passes ~1.4s over 5 runs. Falsified: seeding all 3
relays up front → everything green, only 3 preconditions fail
(`onlyRelayA=false`, `relayBHasIt=true`, `relayCHasIt=true`). Re-signed
lookalike (`created_at + 1`, different id) into C → 3 rows against 2 distinct
events, `maxRows=3`, `sources` stuck at 2 relays, index moved 1→0. No API
finding.

### C4 — reactive derived query: PROVEN LIVE (#1871)

`C4ReactiveDerivedQueryTests.swift`. First `NMPBinding.derived` exercise: a
kind:1 feed of authors projected from a kind:3 contact list's `p` tags.
Publishing a replacement kind:3 makes the feed deliver the new author's notes
with zero app action; ~0.5s. Falsified: a literal set naming both authors from
the start → `feedFollowed=true`, everything else green, caught only by the
phase-1 negative. Never publishing the replacement list → the base-changed
precondition fails. Projecting via `.authors` instead of `.tag("p")` →
resolves to the list's own author and delivers nothing. Untested: whether the
binding retracts rows on an unfollow.

### C5 — replaceable, deletion and stale redelivery: PROVEN LIVE

`C5ReplaceableDeletionStaleRedeliveryTests.swift`. Alice's kind:0 superseded:
one row, v2 id and content. An `["e", target]` kind:5 removes exactly its
target. Stale relays redeliver superseded and deleted events after the app
holds the post-state; a control event proves the stale push was ingest-first
and refused; neither resurrects. ~4.7s. Falsified: writing the stale events
before the supersede/deletion → `resurrected=[]`, only the "already held it"
preconditions fail. Removing kind 5 from the app filter → `deleted=false`,
both notes stay, and the stale relay genuinely resurrects the target. API
finding: the app must subscribe kind 5 itself (NMP will not add it) and must
then filter kind:5 rows out of its own display.

### C6 — deep windowing: PARTIAL, first advance FAILING (#1886)

`C6DeepWindowingTests.swift`. 150 events, `.expandable(initial: 10, max: 100)`,
scrolled to the ceiling against canonical order; ~49s (45s bounded wait). Every
advance but the first passes (exact prefix, extends rather than rewrites, no
duplicates, `.atBound(max: 100)` fact, live arrival displaces the tail).
FAILING: `requestRows(atLeast: 20)` from `initial: 10` returns
`.returned(added: 0)` and the window stays at 10 rows for 45s though the relay
holds all 150; only raising the target again moves it — deterministic 5/5
across `(10,11)`, `(10,20)`, `(10,50)`, `(1,2)`, `(10,10) → (10,11)`. Second
defect: `.returned(added:)` is unreliable — the same advance reports
`added: 20` on some runs and `added: 0` on others; the assertion was removed
rather than inverted. Falsified: dropping event #37 → fails at page 40+,
naming the missing and unexpected ids, index 37 exactly. No-window control:
150 rows delivered against a declared `initial` of 10, 140 unexpected ids on
page 1.

### C7 — normal publish: PROVEN LIVE

`C7NormalPublishTests.swift`. A real signed event via `.explicit(relays:)`; the
row was visible before the relay's confirmation reached the receipt stream on
every run; `sources` then grows on the SAME row; terminal outcome `.settled`.
Passed under 1.5s. Falsified three ways: dead address `ws://127.0.0.1:1` → the
expected timeout; inverting the acceptance-visibility assertion → failed on the
real `Optional(true)` against the wrong `Optional(false)`; inverting the
echo/dedup assertion → failed showing the real `sources` containing the relay
URL. No API finding.

### C8 — publish while relays fail: PROVEN LIVE (#1880)

`C8PublishWhileRelaysFailTests.swift`. 3 destinations, 2 live and 1 `SIGKILL`ed
(`ECONNREFUSED`), ~1.5s. Healthy relays reach `.published`, the dead one stays
`waiting(notConnected)`; `destinations`, the receipt and `publishQueue` all
name all 3; provenance grows to exactly the 2 echoing relays. Lab measurement:
`isReachable(timeout: 2)` needed 2.0057s — the full budget — to report `false`
against a refused port, while the new `RelayHandle.probe` measured `refused` in
0.0001s. Falsified: leaving the third relay alive → the refusal precondition
fails, real history
`["waiting(notConnected)", "waiting(needsAuth)", "sent(attempt=1)", "published"]`.
Dropping the dead destination from routing → `destinations` names 2, receipt
`(never reported)` across 11 facts, queue `[]`. The scenario never reaches
`.settled`, which is correct; `outcome=nil` every run.

### C9 — crash/restart during publication: PROVEN LIVE

`C9CrashDuringPublicationTests.swift`, a real `kill -9` of the sibling
`canary-c9-publisher` at 3 kill points, with recovery through `publishQueue`
enumeration and `reattachReceipt(id:)` only. (1) After local acceptance,
before delivery: recovers, settles, no duplicate. (2) Relay unreachable from
the start: recovers only once healed after the crash. (3) One of 2 relays
already `.published`, the other partitioned: no regression from `.published`
observed on any run — which does not rule out a harmless resend, since the
oracle is `receipt.status` only. All 3 passed 4 consecutive full-suite runs,
~1s each. Falsified: a different store path → `.notFound`; inverting case 3's
no-resend claim → failed on the real `false`. Superseded by #1770:
correlation-token recovery was deleted, and `WriteIntent.correlation`,
`CorrelationToken` and `reattachByCorrelation` were removed entirely.

### C10 — offline write then convergence: PROVEN LIVE

`C10OfflineWriteThenConvergenceTests.swift`. Publish with nothing reachable;
relay restarted on the same port; the write settles itself 7.7–9.9s after
return (~7–12s per run) on the SAME `Receipt`, with no re-publish, no
reattach, and no second engine. The offline row is visible with `sources`
empty; the local key signs with no network; the relay holds exactly one event
by this author. Falsified: leaving the relay up for the write and removing it
only afterwards → `converged=true in 0.3s`, everything green, caught only by
2 preconditions (the port `accepted` at write time, and a second strfry
process over the same LMDB directory finding the event already durable while
the first was dead). Never restarting → stuck at `waiting(notConnected)` for
the full 120s, `outcome=nil`. The duplicate check cannot rule out an identical
resent frame, since the relay deduplicates by id — the same limit as C9.

### C11 — NIP-22 comment capability: PARTIAL, root-shape read-back FAILING (#1876)

`C11CommentCapabilityEndToEndTests.swift`. First capability test against a real
relay: an author's `commentIntent(on: .root(...))` on a NIP-73 web root is
delivered to a reader via `commentThreadDemand(root:)`; both events end up in
the author's own thread query and in a plain `NMPFilter(kinds: [1111])`.
~1.3–2.3s over 5 runs. Falsified: the wrong page → `publishedHere=true` but 0
rows over 4 batches; a dead address → `publishedHere=false` before any
behaviour assertion runs. FAILING (#1876): the demand binds the root to `#I`
for every shape, but the composer writes `E` / `A` for event and address
roots, so ordinary-note comments publish but never read back — a hand-built
`tags: ["E": ...]` filter gets 1 row where the demand gets 0 rows over 6
batches. API finding (#1878): `Nip73.url(url:)` decodes back as
`.general(value:kind:"web")` — the same string in a different `Hashable` case,
so `decoded.root == composed` is `false` for every web thread.

### C12 — identity freeze: PROVEN LIVE

`C12IdentityFreezeTests.swift`, two moments. (1) Unsigned and parked, ~5.8s:
alice (pubkey-only) parks at `awaitingSigner(alice)`; bob gets a key and
becomes current; alice's write signs under alice only once her own key is
added, settling while bob is current — history
`["awaitingSigner(alice)", "inFlight(alice)", "signed(...)"]`. (2) Signed and
undelivered, ~8–10s: alice signs while her relay is refused, bob becomes
current, the relay returns, and the delivered event still carries alice's key.
Both proven at the relay: alice holds one event, bob holds none. Falsified:
giving alice her signer first → green, caught only by `PublishQueueEntry.
outcome` reading `settled` at the switch instant; not switching in case 2 → the
precondition fails; inverting the relay-side claim (asserting the event belongs
to bob) → failed in both directions.

### C13 — relay disconnect/reconnect: PROVEN LIVE (#1863)

`C13RelayDisconnectReconnectTests.swift`. First exercise of two observations
sharing one query — the shape that missed #1848 elsewhere. 6 phases, ~8–10s per
run over 6 runs: (1) both delivered and `observeDiagnostics()` reports exactly
1 wire subscription; (2) the port is `SIGKILL`ed and both reach `disconnected`;
(3) a second strfry process writes an event while the port is provably dead;
(4) both resume with zero app action; (5) closing one sharer leaves the other
receiving; (6) closing the last drops `wireSubCount` 1→0. Falsified: never
closing the last sharer → `wireSubCount` stuck at 1 after 15s, which is #1848;
publishing the outage event before the outage → phase 4 fully green, caught
only by the phase-3 precondition; never restarting the relay → both sharers
stuck holding only the pre-outage id with status `error`. API finding:
`wireSubCount` stays 1 for the whole outage — it counts planned, not live,
subscriptions (#755).

### C14 — NIP-77 reconciliation: PROVEN LIVE (#1888)

`C14Nip77ReconciliationTests.swift`. A feed held open across an outage in which
the relay gains 10 events the local store lacks. Oracle
`RelayDiagnostics.eventsByKind`: negentropy on → 60→70, a delta of 10 for a
10-event gap, and `nip77Handoff` walks
`none → awaiting_live_eose → backfilling → live`. Negentropy off (control) →
also converges on all 70 rows but over a delta of 70 (60→130), with
`nip77Advertisement=advertised_unsupported` and `nip77Behavior` stuck at
`unknown`. Finding: a cold-start reconnect converges "while receiving all 70
events" yet reports `behaviorally_proven` and `none` simultaneously — the probe
verdict lands too late for the in-flight request, so cold start always
refetches and reconciliation needs a later request. Falsified: inverting the
efficiency assertion → real 10 against the wrong 60; seeding the divergence
before the outage → convergence trivially true, caught only by the divergence
precondition. `nip77Handoff` accumulation requires holding `observeDiagnostics()`
open continuously. API finding: no per-query fact distinguishes negentropy
coverage from a plain EOSE; only the transient, engine-global `nip77Handoff`
does.

### C15 — NIP-42 AUTH: PROVEN LIVE, found and closed #1889

`C15NIP42AuthTests.swift`, against strfry `restrictedReadKinds` +
`restrictReadToInvolvedPubkey`. NMP-free probes confirm the preconditions
(unauthenticated → challenge plus `CLOSED auth-required`, 0 events; wrong key →
0 events; involved key → served). NMP itself walks
`connecting → awaitingAuth(awaitingChallenge → awaitingPolicy →
awaitingSignature) → requesting → finishedStoredEvents`; a relay restart yields
a second distinct challenge and epoch; denial surfaces as `authDenied`, and a
later `.allow` recovers without rebuilding. Found #1889, a real deadlock,
committed red first: NMP withheld a protected session's REQs until AUTH but
only built `AuthSessionState` off an inbound challenge that strfry never sends
unsolicited. The control proved it — `authenticateAs: nil` → `status=error`
(the REQ reached the relay), bound session → `status=awaitingAuth(awaiting
Challenge)` (the REQ never sent). The fix: a read session sends its REQs
regardless of identity binding; 3 conditionals removed, none added. Falsified:
the wrong kind in `restrictedReadKinds` → the unauthenticated probe is served
the row and 3 preconditions fail; turning off `restrictReadToInvolvedPubkey` →
the wrong-identity probe is served, failing exactly the identity-scoping
precondition. A strfry writePolicy-plugin approach was a false start — it
denies writes but never calls `sendAuthChallenge()`, so no AUTH frame is
emitted; a NIP-70 `-` tag was used instead.

### C16 — slow consumer and backpressure: PROVEN LIVE (#1869)

`C16SlowConsumerBackpressureTests.swift`. 400 events every 25ms against a
reader sleeping 1s per delivery, with batches-delivered against
batches-published as the oracle. Stable over 4 runs: the slow reader takes
13–14 batches, the fast control 102–108, and both end holding all 402 ids with
no duplicates and no loss. Peak heap difference between the arms: +51,920 /
+77,712 / +96,608 / +97,520 bytes. Currency turned out to be the wrong axis —
a reader pulling 8× less often still held 389 of 401 ids at flood end, because
every delivery is a full snapshot; the committed precondition therefore uses
read count plus deepest backlog (36–40 unseen ids slow against 4–7 fast).
Falsified: zero delay → 102 deliveries against a bound of 50; dropping 1 row in
10 in the reader → 361 against 402 on both id-set checks. No API finding; the
iterator throttles to ~60/s regardless (#17).

### C17 — repeated lifecycle churn: PARTIAL, `distinct` phase FAILING (#1846)

`C17RepeatedLifecycleChurnTests.swift` with the sibling `canary-c17-churner`
(#1796). PASSING `repeat` (300 identical-filter cycles): fds flat at 8, threads
flat at 18, heap +6 bytes per cycle; cross-length 74,456,848 B at 300 against
74,456,400 B at 1200, i.e. −0.5 B per cycle, no growth. PASSING `engine` (60
construct / `shutdown()` cycles): post-shutdown heap 177 KB flat against ~74 MB
live, fds 6→3. FAILING `distinct` (300 cycles, a distinct filter each time):
heap +402 B per cycle; cross-length 74,577,904 B at 300, 74,840,160 B at 1200
(+291 B per filter), 76,354,832 B at 4000 (+541 B per filter) — linear, no
plateau; `phys_footprint` over 4000 cycles rose 9.49 MB → 13.78 MB. Filed
#1846 and left red (the bound was set at 128 B per cycle against measured noise
of +22 B at 300 and +41 at 1200). Falsified: leaking 1 fd per cycle → +1.00 fd
per cycle exactly, 9→68 over 60 cycles; leaking every `NMPQuery` → heap
+22,592 B per cycle against a clean +22 B per cycle baseline, footprint
+22,518 B per cycle, and `wireSubCount` after close stuck at 1. API findings:
`observeDiagnostics()` has no synchronous snapshot read, and
`DiagnosticsSnapshot` exposes nothing about the #1846 retention.

### C18 — clean shutdown: PROVEN LIVE (#1870)

`C18CleanShutdownTests.swift` with the sibling `canary-c18-quitter`.
Outside-process preconditions: `lsof` shows 2 ESTABLISHED TCP connections on
the child's pid, and a second `NMPEngine` over its store path throws
`NMPError.storeAlreadyOpen` while it lives and succeeds after it exits. QUIT to
real exit takes 0.03–0.04s with `terminationReason == .exit` and status 0.
fds/threads: baseline 3/15 → live 14/30 → after teardown (`explicit` 6/15,
`implicit` 14/30) → after release both 3/15, with `implicit` never calling
`shutdown()` and still landing on baseline. Falsified: comparing only "below
live" let a deliberately leaked engine pass at 11/18, so the assertion was
changed to compare against the app's own baseline and then correctly failed at
11/18 against 3/15; a hung teardown (120s) → the app was `SIGKILL`ed after 90s
with `terminationReason=uncaughtSignal` and status 9; a dead address → 0 TCP
connections, 0 relay-sourced rows, no durable read-back.

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
