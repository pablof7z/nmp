# The reference application

One small, real application whose job is to keep NMP honest as a product.

The reference app is not a reducer harness, a mock-heavy integration test, an
API showcase, or an architecture demo. It behaves like an ordinary downstream
application and uses only the APIs an actual application is expected to use.
It answers, continuously and empirically:

> Can a normal application actually use NMP to read, publish, recover,
> reconnect, and remain fast and understandable under realistic conditions?

It exists because every other layer can be green while the public API is
awkward, an advertised flow does not work end to end, restart breaks only with
real resources, relay work explodes, resources are pathological, a capability
needs app-side lifecycle hacks, or an internal concern leaks into application
state.

## Non-negotiable realism

The app uses: the supported public `nmp` facade; the real runtime; the real
Redb persistence path; real OS threads/tasks; real TCP and WebSocket
connections; real Nostr frames; real signatures from a real local signer; real
process shutdown and restart; and **real relay implementations launched as
separate processes**.

It does not use: `nmp-engine`, `nmp-runtime`, `nmp-store`, router/resolver
internals, fake engine or store implementations, direct Redb inspection to
decide whether the app is correct, privileged commands a production
application cannot issue, or any test-only constructor that bypasses normal
lifecycle rules.

The app must not know whether NMP internally uses a query engine, a publish
engine, request attempts, coverage maps, or lanes. **If a flow cannot be
implemented without that knowledge, that is an NMP API finding, not an app
problem.**

Treat awkward app code as a product bug until shown otherwise, and do not add
helper abstractions inside the app to conceal an awkward NMP API. A little
duplication is preferable to hiding evidence.

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
- **NIP-42 write denial is real**, with a machine-readable
  `auth-required: this relay requires NIP-42 auth for writes`. Fresh challenge
  per connection confirmed in `RelayIngester.cpp`: a new `challengeGenerator`
  is minted per connection id, which is exactly the reconnect property the AUTH
  scenario needs.

Rejected alternative: **khatru** is a library rather than a relay binary (you
write your own `main.go` and wire up storage yourself) — more integration work
for no proven advantage.

### Why an in-process relay cannot be the system under test

A relay that is a real WebSocket on a real port, with a genuinely real backend,
can still be disqualified: if it runs **inside the same OS process** as its
caller and has **no database file**, its "restart" is a fresh in-memory
instance on the same port. It cannot prove that anything survives a restart,
because there is nothing on disk to fail to read back. That is a property of
its design, not a bug in it.

Such a relay remains the right fast fixture for narrow checks. It is
disqualified as the reference app's system under test.

### The loophole to watch for

Wrapping an in-process relay in a thin binary and invoking it as a subprocess
produces something that satisfies the *letter* of "separate process" while
being exactly the in-process, non-persistent fake this document forbids as the
system under test.

**Review must check what is inside the process — does it persist across
kill+restart, is it a real third-party relay codebase — not merely that a
second PID exists.**

### What is enforced, and what is only reviewed

Living in-repo means the app loses guarantees a separate repository would have
had for free. Worth naming, because this is where erosion starts.

Enforced by construction:

- The relay is reached only by a `ws://` URL to a separate process. No
  compile-time or link-time edge exists from the app to NMP's internals; the
  relay binary has no dependency edge into NMP at all.
- Seeding happens over real `EVENT` frames, so there is no direct
  insert-into-the-database door on the relay side to be tempted by.

Policed by review only:

- Nothing prevents the app from reaching past the public `nmp` facade into the
  mechanism crates. A separate repo consuming a published crate could not do
  this; an in-repo app can.
- Nothing prevents a scenario author from asserting against the relay's
  exported database instead of driving the app through the public facade.
- The harness loophole above.

The relay binary itself is built by a documented, commit-pinned script into a
gitignored cache directory outside the repository entirely. It is **not**
vendored: a committed platform-specific binary is dead weight in git history
forever, and a build script is small text that reproduces it on demand. Note
honestly that a clean machine also needs the Homebrew dependency line, which
costs real time beyond the strfry build itself.

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

## The scenarios

Seventeen scenarios, each covering one thing an ordinary app has to survive:

C1 cold start and live feed; C2 cache then offline restart; C3 multi-relay
dedup and provenance; C4 reactive derived query; C5 replaceable, deletion and
stale redelivery; C6 deep windowing; C7 normal publish; C8 publish while
relays fail; C9 crash/restart during publication; C10 offline write then
convergence; C11 capability end to end (NIP-22 comments); C12 identity
freeze; C13 relay disconnect/reconnect; C15 NIP-42 AUTH; C16 slow consumer
and backpressure; C17 repeated lifecycle churn; C18 clean shutdown.

There is no C14. It was NIP-77 negentropy reconciliation, and negentropy was
deleted from the tree (`72df7f96`); the numbering is not reassigned, because
the scenario labels appear in issue history.

C11 was renamed (#1875). It was listed as "semantic capability" — a phrase
that appeared exactly once in this entire repository, introduced by the commit
that created the app. No definition anywhere, no matching public API, and no
protocol category by that name. The name now comes from the public surface:
"capability" is the engine's own feature-key word, and "NIP-22 comments" is the
protocol's name.

**The numbers live in the scenario files, not here.** Each scenario states its
corpus size, cadence, cycle counts and committed bounds, and several state what
their bound cannot resolve — a churn bound roughly three times the measured
noise floor cannot see a smaller leak, and should say so. Timings, byte counts
and run counts are deliberately kept out of this document: repeated in prose
they drift away from the assertion, lose the caveat that made them honest, and
read as evidence when they are a recollection. Run the scenario.

## What the reference app must not become

A generic testing framework, a huge demo application, an internal NMP
debugger, a mock relay, a protocol simulator, a synthetic benchmark detached
from app behaviour, a second implementation of NMP semantics in app code, a
dashboard project, or a new source of architecture policy.

If implementing or running it becomes more complex than understanding the NMP
behaviour it tests, simplify it.

## Related

- `docs/known-gaps.md` — unproven scenarios and unexplained failures stay
  visible there until resolved.
