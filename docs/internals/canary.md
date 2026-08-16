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

### Where the controller lives

One implementation, two consumers. The lifecycle logic — start, stop, kill,
restart, partition, heal, seed, ephemeral port, isolated temp directory,
bounded-poll readiness rather than sleeps — belongs in a small library target
under `apps/Canary`. It is consumed by a thin CLI a developer runs to bring
the lab up, and by the app's own test target, so there is never a second
hand-rolled notion of "what a real relay process lifecycle looks like".

The relay binary is built by a documented, commit-pinned script into a
gitignored cache directory. It is **not** vendored: a committed
platform-specific binary is dead weight in git history forever, and a build
script is small text that reproduces it on demand. Note honestly that a clean
machine also needs the Homebrew dependency line, which costs real time beyond
the strfry build itself.

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

## Nothing in this app has ever been compiled

As of 2026-08-16, **no Swift source in `apps/Canary` has ever been type-checked
against the real `NMP` module in this worktree.** `Packages/NMP/NMP.xcframework`
is gitignored and generated by `scripts/build-swift-xcframework.sh`; it does not
exist here. So `xcodebuild` fails while resolving the target dependency graph,
before it schedules a single Swift compile action — zero `.o` files, zero
`.swiftmodule`.

This corrects commit `8575fa08`, whose message claims `xcodebuild` "reaches the
link stage with no Swift compiler error". That is false, and it is false in the
direction that flatters: "reaches the link stage" implies the source
type-checked and only the final binary was missing. Nothing type-checked. A
merged commit message cannot be amended, so the correction lives here, where
someone would actually look.

What does exist: `xcodegen generate` succeeds against the project spec, and
`xcrun swiftc -parse` passes on every source file — syntax only, no module, no
type-checking. Call sites were hand-verified line by line against the real
signatures, which is careful work and is not a compiler agreeing.

**Until the xcframework is built for a simulator destination, treat every
Canary claim as unproven** — the identity-override picker, the Keychain
round-trip, the reattach-after-restart path, and whether the app compiles at
all.

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

- `docs/known-gaps.md` — unproven scenarios and unexplained failures stay
  visible there until resolved.
- `skills/nmp-dev/references/testing/` — the three-layer testing model the
  Canary is the third layer of. A green result at one layer never substitutes
  for a missing layer.
