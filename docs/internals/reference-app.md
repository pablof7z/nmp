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

It lives at `crates/nmp-canary`: a NIP-29 rooms client where the people in
rooms are portable identities you can follow off them. That shape was chosen
because it puts two contradicting routing authorities inside one app — a
NIP-29 event is pinned to its host relay by protocol, a kind:1 is routed by
NIP-65 outbox to a discovered relay set — and sometimes inside one composed
draft. `src/bin/canary.rs` is the exerciser.

**The findings are the deliverable.** Each module opens with what the author
wanted to write and what they had to write instead, and `findings` carries the
same content as ranked data so the exerciser can print it and nothing has to be
taken on trust. Where a suspicion turned out to be false, the module records
that too, with the code that refutes it.

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

The dependency list is itself evidence. `crates/nmp-canary` depends on `nmp`
plus one line per capability, as intended, and deliberately does **not** depend
on `nostr` or `nmp-grammar`. Every place where naming one of them would have
been easier is recorded as a finding instead. The two dependencies that are
neither — `serde_json` to read a kind:0 profile, `tokio` to await a group
observation — are recorded as findings too rather than waved through.

The exerciser is a binary and not a test, and that is load-bearing. It spawns
and SIGKILLs child processes because several scenarios cannot be expressed any
other way: a restart is only a restart if the writing process exited, since a
second `Engine` over one store in one address space still holds the redb pages,
the allocator and every decoded row; a crash is only a crash under SIGKILL,
with no `shutdown` and no `Drop`; descriptors, threads and resident size are
properties of a process; "the process exited" and "teardown returned" are
different signals; and two processes contending for one store is not a function
call.

The app must not know whether NMP internally uses a query engine, a publish
engine, request attempts, coverage maps, or lanes. **If a flow cannot be
implemented without that knowledge, that is an NMP API finding, not an app
problem.**

Treat awkward app code as a product bug until shown otherwise, and do not add
helper abstractions inside the app to conceal an awkward NMP API. A little
duplication is preferable to hiding evidence.

## The relay lab

**Outstanding.** `crates/nmp-canary` currently runs against a real `nmp::Engine`
with a local store and **no reachable relay**: every write settles as
`NoDestination` and every read is served from the canonical local store. That
is enough to exercise the surface, which is what the crate exists to measure so
far, and the crate says so in its own header. The relay half is a separate
harness that does not exist yet — `ls crates/ | grep -i relay` returns nothing,
and `grep -rni strfry crates/nmp-canary/` returns 0.

The rest of this section is the standing requirement that harness must meet,
not a description of something running today.

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

`cargo run -p nmp-canary --bin canary [scenario]`, where the scenario is one of
`surfaces`, `deletions`, `routing`, `restart`, `crash`, `contend`, `teardown`,
`findings`, or `all` (the default). The `child-*` forms are spawned by the
supervisors and are not meant to be typed.

**The numbers live in the scenarios, not here.** Timings, byte counts and run
counts are deliberately kept out of this document: repeated in prose they drift
away from the assertion, lose the caveat that made them honest, and read as
evidence when they are a recollection. Run the scenario.

### The C-numbered roster is history

The deleted Swift application had a different plan — eighteen numbered
scenarios, C1 through C18 — and the numbers still appear in issue history, so
they are worth being able to resolve. Two things about that roster were
recorded wrongly and are corrected here rather than carried forward:

- It claimed all eighteen scenario files existed. Seventeen did:
  `git ls-tree fd839931^ apps/Canary/CanaryScenarios/Tests/CanaryScenariosTests/`
  lists C1–C13 and C15–C18.
- There was a C14, for NIP-77 negentropy reconciliation, whose subject was
  deleted from the tree in `72df7f96`. `grep -ril 'nip77\|negentropy' crates/`
  returns 0 against a control of 6 for `nip42`.

C11 was also renamed (#1875) off "semantic capability" — a phrase that appeared
exactly once in the repository, introduced by the commit that created the app,
with no definition, no matching public API and no protocol category by that
name.

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
