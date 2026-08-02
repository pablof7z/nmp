# NIP-29 real-relay consumer capstone

Status: **passed and merged** on `master` at `0ef5e1c7`.

This is the completion report for #1140 and #1203. It records what the direct
Rust and downstream Swift consumers proved through NMP's public facade. The
two Croissant processes, `nak`, and their logs were fixture infrastructure and
independent relay witnesses only. No fixture result is treated as an NMP
product result.

## Landed work

| Unit | Issue | Merged PR | Result |
|---|---:|---:|---|
| unrestricted two-relay fixture and signed seed | #1200 | #1204 | one-command Docker and host-process backends |
| direct Rust consumer | #1201 | #1205 | public `nmp` facade proof |
| downstream Swift consumer | #1202 | #1208 | hand-written `NMP` wrapper proof; no `NMPFFI` import |
| live/restart adversarials | #1209 | #1213 | Rust and Swift proof on the final tree |
| per-relay NIP-77 provenance repair | #1216 | #1221 | product defect found by the capstone and fixed |
| host relay restart ownership | #1219 | #1220 | fixture lifecycle defect found by the hosted run and fixed |

The final exact-head Swift run is the hosted macOS qualification job for #1213:

<https://github.com/pablof7z/nmp/actions/runs/30769898598/job/91555122177>

## Topology and seed

The harness pins Croissant commit
`9c4c93e84852bd9aa6824060b74c56ab2ce812c2` and starts isolated relays at
`ws://127.0.0.1:19888` and `ws://127.0.0.1:19889`. Both are open and
unrestricted; there is no supported-kind allow-list. The application-selected
kind 9 and kind 30023 reads therefore test NMP's kind-blind group door rather
than a relay allow-list.

The signed seed contains:

- `solo-a`, hosted only by relay A;
- `bitcoin`, independently hosted by A and B with conflicting metadata and
  different membership evidence;
- `one-sided`, hosted only by A to force one ACK and one typed rejection for a
  two-host write;
- a viewer contact list and positive member-list evidence for follows-derived
  discovery;
- one identical kind 9 event and one identical kind 30023 event published to
  both relays; and
- 24 kind 9 stress rows with dense timestamp ties, split across A and B.

The retained run `nmp-nip29-harness-1209b` independently observed this
post-seed distribution:

| Kind | Relay A | Relay B |
|---:|---:|---:|
| 3 | 1 | 1 |
| 9 | 15 | 14 |
| 30023 | 2 | 2 |
| 39000 | 3 | 1 |
| 39001 | 3 | 1 |
| 39002 | 3 | 1 |

Its seed summary contains one shared event ID for kind 9 and one for kind
30023. The fixture-hardening follow-up #1223 records that future setup should
also fail immediately if either intersection is empty. That does not invalidate
these runs: their summaries contain both IDs and both NMP consumers observed
two-source provenance.

## Replay

From the repository root, using two paths that do not already exist:

```sh
tools/nip29-consumer/run-capstone.sh \
  /tmp/nmp-nip29-rust-run \
  /tmp/nmp-nip29-rust-evidence
```

On macOS with Xcode and the host relay backend:

```sh
NMP_NIP29_HARNESS_BACKEND=host \
  tools/nip29-consumer-swift/run-capstone.sh \
  /tmp/nmp-nip29-swift-run \
  /tmp/nmp-nip29-swift-evidence
```

Each runner starts and seeds both unrestricted relays, runs the consumer
phases, captures public proof lines and NMP stores, stops both relays, and
verifies that ports 19888 and 19889 are released. Signing secrets remain only
inside the private disposable run directory and are never copied into the
consumer evidence or this report.

## Product results

The Rust capstone passed twice end to end after the final adversarials were
added. The hosted Swift capstone passed on exact head `cd446917`, which became
merge commit `0ef5e1c7`.

| Claim | Rust | Swift |
|---|---|---|
| single-host group | 1 kind 9 row, source A only | 1 kind 9 row, source A only |
| app-selected kind 9 union | 27 distinct rows; shared row has 2 sources | 27 distinct rows; shared row has 2 sources |
| app-selected kind 30023 union | 3 distinct rows; shared row has 2 sources | 3 distinct rows; shared row has 2 sources |
| slow consumer | one delayed frame carried 27 deltas without semantic loss | delayed consumption still reached 27 rows and 2 sources |
| bounded window | initial 3, grown to 8, maximum 8 | initial 3, grown to 8, maximum 8 |
| follows-derived discovery | positive member-list evidence found `bitcoin` from A | same predicate and relay-specific result |
| metadata disagreement | both rows retained; app chose B's `Bitcoin (real)` | same two rows and app-owned policy |
| exact diagnostics | A: 2 filters; B: 1 filter; per-kind counts and coverage retained | one group filter per relay during the group diagnostic observation |
| normal kind 9 write | A ACK, B ACK | A ACK, B ACK |
| normal kind 30023 write | A ACK, B ACK | A ACK, B ACK |
| mixed `one-sided` write | A ACK; B typed group-not-found rejection | A ACK; B typed group-not-found rejection |

The Rust diagnostic snapshot reported relay A event counts
`3:1, 9:19, 30023:2, 39000:1, 39001:1, 39002:1` and relay B counts
`3:1, 9:17, 30023:2, 39000:1, 39001:1, 39002:1`. These are NMP diagnostic
values, not counts copied from `nak`.

## Adversarial and lifecycle results

| Scenario | Result |
|---|---|
| metadata changes after observation opens | A became `Bitcoin Cash live`; B became `Bitcoin (real) live`; both remained inspectable |
| follows replacement | removing the followed subject retracted `bitcoin`; restoring it re-added `bitcoin` without replacing the observation |
| identical observation sharing | two observations held one group filter per relay |
| first cancellation | one group filter per relay remained and the survivor received a new chat with two-source provenance |
| last cancellation | group filter count became zero on both relays |
| provenance growth | shared row started with A only while B was down, then grew to A+B after B returned; Rust emitted `RowDelta::SourcesGrew` |
| offline store reopen | 28 cached rows, two sources on the shared row, and per-source watermarks survived with both relays offline |
| conflict reopen with one source offline | both relay-specific live metadata rows and both cached sources survived; offline B was `Connecting` while A was `Requesting` |
| sibling recovery | B returned to `Requesting` and both metadata branches refreshed without replacing the observation |
| teardown | explicit cancellation and engine shutdown; both relay processes/containers stopped; ports 19888/19889 released |

The hosted Swift proof lines include:

```text
PROOF swift_live_mutation ... follows_removed=true surviving_chat_sources=2 ...
PROOF swift_live_follow_readded group=bitcoin observation_reused=true
PROOF swift_shared_cancellation ... after_one=1/relay ... after_last=0/relay
PROOF swift_restart_conflict_offline ... cached_sources=2 ...
PROOF swift_provenance_before shared_sources=1 relay_b_content=false
PROOF swift_provenance_after shared_sources=2 relay_b_content=true ...
PROOF swift_restart_offline cached_rows=28 shared_sources=2 persisted_watermarks=true ...
NMP NIP-29 Swift capstone passed
```

## Compiling consumer excerpts

These are contiguous excerpts from the applications that compiled and ran in
the proofs above.

### Rust discovery

```rust
let follows = Binding::Derived(Box::new(Derived {
    inner: follows_demand,
    project: Selector::Tag("p".to_string()),
}));
let predicate = nip29::member_list_includes(follows);
scope.groups_where(&predicate).map_err(display)
```

### Rust feed observation

```rust
let chat_subscription = context
    .engine
    .observe(group.read(kinds([9])).map_err(display)?, None)
    .map_err(display)?;
let mut chats = Observed::default();
wait_until(&chat_subscription, context.settle, &mut chats, |observed| {
    observed.rows_of_kind(9).len() >= 27
        && observed.has_source_count(SHARED_CHAT, 2)
})?;
```

### Rust publication

```rust
let chat = group
    .publish(
        &context.engine,
        context.writer,
        EventBuilder::new(Kind::from(9u16)).content("NMP consumer published chat"),
    )
    .map_err(display)?;
let chat_statuses = wait_for_write(&chat, context.settle, |statuses| {
    acked(statuses, &context.relay_a) && acked(statuses, &context.relay_b)
})?;
```

### Swift discovery

```swift
let follows = NMPBinding.derived(
    inner: NMPDemand(
        selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
        source: .pinned(Set([context.args.relayA, context.args.relayB])),
        cache: .strict
    ),
    project: .tag("p")
)
let predicate = try NMPGroupPredicate.memberListIncludes(follows)
let query = try context.engine.observe(context.scope.groupsWhere(predicate))
```

### Swift feed observation

```swift
let chatQuery = try context.engine.observe(group.read(NMPFilter(kinds: [9])))
let chats = try await waitForRows(chatQuery, seconds: args.settleSeconds) { batch in
    let kindRows = rows(batch, kind: 9)
    let sharedSources = kindRows
        .first(where: { $0.content == sharedChat })?.sources.count ?? 0
    return kindRows.count >= 27 && sharedSources == 2
}
chatQuery.cancel()
```

### Swift publication

```swift
let chat = try group.publish(
    engine: context.engine,
    authorPubkeyHex: context.writer,
    kind: 9,
    content: "Swift NMP consumer published chat"
)
defer { chat.cancel() }
let chatStatuses = try await waitForStatuses(chat, seconds: context.args.settleSeconds) {
    acked($0, relay: context.args.relayA) && acked($0, relay: context.args.relayB)
}
```

Neither consumer constructs an `h` tag, resolves a group route, opens a relay
connection, owns a subscription registry, merges caches, infers source
evidence, or retries a write.

## Application-side cost

The Rust consumer is 1,091 source lines across four files. Its `Cargo.toml`
depends only on `nmp`. Its app-owned model is limited to argument parsing, one
`Context`, and an `Observed` accumulator that applies public `Frame` deltas for
assertions.

The Swift consumer is 809 source lines across five files. It imports only the
hand-written `NMP` package. Its app-owned support consists of arguments/modes,
one probe context, timeout/status wait helpers, and one actor coordinating the
fixture's live mutation checkpoints.

Most of those lines are falsifier assertions and machine-readable proof
printing. No protocol/network/cache/routing/liveness abstraction was required
in either application. Display-winner selection is intentionally app policy;
it chooses relay B's metadata without deleting relay A's evidence.

## Defects and honest limits

The capstone found one NMP product defect: NIP-77 reconciliation was seeded
from the relay-agnostic store. If one relay delivered a shared event first, a
second relay was told that NMP already had the ID, so its copy was never fetched
and its provenance was never recorded. #1221 scopes the local reconciliation
snapshot to rows already seen from the relay being reconciled. Its deterministic
headless test was red before the fix; afterward `cargo test -p nmp`, 30/30
optimized direct-FFI/Redb runs, and hosted Swift all passed with two sources.

The hosted run also found a fixture-only lifecycle race: `relay-down` attempted
to shell-`wait` for a process owned by an earlier shell and returned before the
process exited. #1220 replaced that with ownership-aware exit polling and
preserved logs for genuine early exits.

No required capstone scenario was skipped. The open #1211 BDD quiet-window
flake is a separate 300-group scripted-world settlement problem, not a skip or
failure in these real-relay consumers. #1223 is a nonblocking fixture
failure-hardening follow-up. There is no known remaining NMP product gap exposed
by this capstone.

## Retained evidence and integrity

The second final Rust run is retained locally in
`/var/tmp/nmp-nip29-harness-1209b` and
`/var/tmp/nmp-nip29-evidence-1209b`. Selected scrubbed artifact hashes are:

```text
f05d1325d38a23cbf4c53e5a337547ecb94e25a0f304fa2059cd38c2839aecb1  manifest.json
76ddc789c5b94ce0aa722a786263d3114c33073f59efef046516b5abcb1113b6  seed/summary.json
ec47605b5d6e3d4cb140b1624c0509dacb9cc0326a4c3b9c0d2a5a1c1157e4c5  seed/relay-a.jsonl
7eeea0e3108ad7b1cc2bcacdad255ccf448417be27c1411ddcdb891900cc7b37  seed/relay-b.jsonl
c87d14043ee4cb3325b2b2f8332bce270a35ccbb2cb360d7a0242111f45a163e  relay-a/events.log
c75236cc9eb9aa6403f7a1f488bd4b0ec6997fe9a5ba60ef1bd8f9a519c54d6c  relay-b/events.log
868b46e68e11533251b1fe10b52b19c33f3474714b1c8a736f06b7d9e6a6de5f  proof-lines.txt
6a3e47da20d1012f12a4e93d611453583a5d85bd27da820f5fe870a75b5d74b6  harness-stop.log
```

The stop record says both containers were removed and ports 19888/19889 were
released. A post-run socket/process check also found neither listener nor a
Croissant process on those ports. The hosted Swift job log is the durable
exact-head witness for its public proof lines and successful package tests.

## Completion audit

| #1140 requirement | Verdict | Evidence |
|---|---|---|
| two relays start and seed with one command | pass | #1204; both backends; pinned commit and manifest |
| Rust required and adversarial scenarios | pass | #1205, #1213; two final real-relay runs |
| Swift cross-FFI scenario set | pass | #1208, #1213; hosted macOS exact-head job |
| no app network/cache/routing/liveness workaround | pass | dependency/import audit and source review |
| truthful feature/evidence state | pass | behavioral traceability and architecture checks green on #1213 |
| discovery/subscription/publication independently auditable | pass | compiling excerpts and proof lines above |
| runtime artifacts and teardown retained | pass | local witness/evidence paths, hashes, hosted job, released ports |
| every child issue closed and PR merged | pass | #1200, #1201, #1202, #1209 closed; #1203 closes with this report |

The clean-consumer falsifier therefore supports the NMP thesis for the tested
WIP NIP-29 surface: applications declare live queries and write intents while
NMP owns routing, transport, deduplication, cache/restart, liveness, scoped
evidence, provenance growth, bounded delivery, cancellation, and per-relay
write outcomes.
