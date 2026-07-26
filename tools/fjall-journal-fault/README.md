# Fjall journal-write-error falsifier (#818)

Permanent regression proving how three pinned Fjall releases behave when a
**real** journal write fails at the transaction boundary.

The regression runs as part of the ordinary workspace test lane:

```sh
cargo test --workspace                        # includes this regression
cargo test -p fjall-journal-fault-harness     # just this regression
./scripts/check-fjall-journal-fault.sh        # plus fmt/clippy for the detached probes
```

Tracks #818, under #701 and storage epic #698.

## Why this exists

`fjall::batch::Batch::commit` writes the batch to the journal, then applies it
to the memtables. The two lines that matter:

```rust
// 3.1.6
let _ = journal_writer.write_batch(self.data.iter(), self.data.len(), batch_seqno);

// 3.1.7 and 3.1.8
journal_writer.write_batch(self.data.iter(), self.data.len(), batch_seqno)?;
```

`write_batch` itself is byte-identical across all three releases; the defect and
its repair live entirely at that call site. Source inspection narrows the risk
but cannot show what a caller actually observes, so this measures it.

Fjall 3.1.8 is the **candidate**. 3.1.7 is where the fix entered and 3.1.6 is the
negative control. The candidate's result is observed directly — it is never
inferred from 3.1.7 by semver.

## What passing does and does not mean

Passing qualifies exactly one behaviour of the pinned 3.1.8 build: an
acknowledged transaction is not silently unrecoverable when the journal write
fails. It does **not** qualify Fjall's semantics, maintenance, performance, or
production readiness, and it does not select a database. Redb remains NMP's
production backend, and NMP's production constructors stay Redb-only.

## Layout

| Path | Role |
| --- | --- |
| `shared/probe.rs` | The probe body, compiled verbatim against all three releases |
| `v3_1_6/`, `v3_1_7/`, `v3_1_8/` | One package per pinned release, each with its own committed lockfile |
| `harness/` | Builds and runs the probes, parses evidence, asserts the matrix |

`harness/` is a member of the NMP workspace, so `cargo test --workspace` runs
the regression and it cannot rot. That is safe because the harness depends on no
Fjall crate: each `v3_1_*` package sets an empty `[workspace]`, so the three
pinned releases stay detached and are built and run as child processes. No Fjall
version enters NMP's production or default feature graph.

Because the probe packages are detached, the repo-wide `cargo fmt --all` and
`cargo clippy --workspace` never see them; `scripts/check-fjall-journal-fault.sh`
lints them.

Three packages exist because the three releases cannot coexist in one dependency
graph. Each pins `lsm-tree` explicitly as well: Fjall depends on it with a caret
range, so without that pin a 3.1.6 probe would silently link `lsm-tree` 3.1.8 and
the recorded checksum would not describe what ran.
`pinned_release_identities_match_the_recorded_evidence` asserts every committed
lockfile still carries the exact crate identities recorded in #818, and the
harness builds with `--locked`, so a substitution fails the run instead of
quietly re-resolving.

## The fault

A real filesystem write failure, not an error returned from probe-owned code.

After the baseline is written and synced, `RLIMIT_FSIZE` is armed to just above
the journal's current write offset. The next journal extension crosses it: Linux
writes up to the limit, then fails the following `write(2)` with `EFBIG` and
raises `SIGXFSZ` on the writing thread.

The fault is **one-shot** — the soft limit is raised by the first fault, so the
later `PersistMode::SyncAll` path stays healthy. This is essential, not a
detail: see the `persistent` control below.

`setrlimit` is not async-signal-safe. The `SIGXFSZ` handler therefore performs
only async-signal-safe `write`/`read` on a pre-created pipe pair; a helper
thread raises the soft limit and acknowledges, and only then does the handler
return to Fjall. Each release runs in its own child process because `RLIMIT` and
signal disposition are process-global.

The journal write offset is discovered by scanning for the end of the non-zero
prefix — Fjall pre-allocates the journal to 64 MiB of zeros and writes forward
from offset 0.

## Observed results

Linux 6.1 / Debian, 2026-07-26. Twelve pre-state rows across three keyspaces;
target transaction 12,488 bytes across the same three keyspaces (journal buffer
is 8,192 bytes, so `write_batch` must reach the file descriptor). Exactly one
`SIGXFSZ` in every armed run. Row counts below are shorthand — the harness
compares exact keys and values.

### `one-shot` — the regression

| Release | `commit()` | In-process | Reopen ×2 |
| --- | --- | --- | --- |
| 3.1.6 | `Ok(())` | 24 rows — every target key visible | 12 rows = exact pre-state |
| 3.1.7 | `Err(Io(EFBIG))` | 12 rows — no partial state | 12 rows = exact pre-state |
| 3.1.8 | `Err(Io(EFBIG))` | 12 rows — no partial state | 12 rows = exact pre-state |

3.1.6 is the acknowledged-loss counterexample: `commit` returns success, all
batch keys are live in-process, the journal holds only an incomplete final
batch, and reopen returns the exact pre-transaction state. This is the shape
#818 anticipated; it was recorded as observed, not assumed.

`Io` rather than `Poisoned` is what proves the error came from `write_batch`
and not from the later persist path.

### Controls

| Control | Purpose | Observed |
| --- | --- | --- |
| `healthy` | The fixture, not the injection, is sound | All three commit and reopen to the same exact post-state; zero signals |
| `persistent` | A later persistence failure must not stand in for this result | 3.1.6 → `Poisoned`; 3.1.7/3.1.8 → `Io(EFBIG)` |
| `undersized` | A batch below the journal buffer never reaches the fd inside `write_batch` | All three → `Poisoned`, indistinguishable |
| `misinjected` | A fault aimed away from the journal must be refused | Fault consumed on a scratch file; all three commit and the target survives reopen |

The `persistent` control is the reason the primary fault must be one-shot: under
a persistent fault **3.1.6 also returns an error**, so a regression built on a
disk-full style fault would wrongly conclude 3.1.6 is safe.

The `undersized` control shows the same trap from the other side — with the
record still inside the `BufWriter`, the fault lands on the `persist` flush and
every release reports the identical `Poisoned`.

That control also surfaced a separate property, now measured rather than
assumed: the commit is **rejected** with `Poisoned` and leaves nothing live
in-process, yet the target rows are **present after reopen**. The record was
still buffered, the one-shot handler raised the soft limit when the fault fired,
and the buffered record therefore completed during shutdown and was replayed.
(The earlier wording here blamed the probe's explicit disarm; the actual cause is
the one-shot handler's raise, which happens first.)

Verified against the alternative: with the fault **sustained** through close and
reopen, the same rejected commit reopens to the exact 12-row pre-state. Both
variants return `Poisoned` and both stop at journal offset 1190 before drop, so
the difference materialises only at shutdown.

| undersized variant | `commit()` | reopen ×2 |
| --- | --- | --- |
| fault lifted (this control) | `Err(Poisoned)` | 15 rows — rejected transaction is durable |
| fault sustained | `Err(Poisoned)` | 12 rows — exact pre-state |

So a rejected transaction can become durable, **conditional on the underlying
fault clearing before shutdown**. This is not a claim about Fjall under a
sustained fault, and it does not touch the one-shot `write_batch` lane, where the
record is truncated mid-batch and every release reopens to the pre-state.
[#821](https://github.com/pablof7z/nmp/issues/821) owns the resulting backend
error contract and its oracle consequence;
`undersized_batch_is_refused_rather_than_silently_passing` asserts the exact
reopened state so the behaviour cannot change unobserved. It is also one more
reason a persist-path failure is not interchangeable with the one-shot
`write_batch` result.

Refusals — more than one injected failure, a fault that missed the commit,
journal rotation, or a batch that is not actually over/under the buffer — fail
the run rather than passing quietly.

## The falsifier is itself falsified

Requirement: *the reproducer fails if the 3.1.7 error propagation is changed
back to 3.1.6 behavior.*

Two independent mechanisms discharge this:

1. The 3.1.6 lane **is** that behaviour, and the one-shot test asserts the
   candidate's commit result differs from it.
2. Directly verified on 2026-07-26 by patching a local copy of Fjall 3.1.7 to
   restore `let _ = journal_writer.write_batch(..)`. The probe then reproduced
   3.1.6's shape exactly (`COMMIT_RESULT=ok`, 24 rows live, 12 rows after
   reopen) and the harness failed with
   `fjall 3.1.7: the journal write error was not returned`.

## Platform support

Linux is the supported lane and CI executes the real fault there. Other
platforms are typed as unsupported with a stated kernel/filesystem reason rather
than skipped silently, and the regression as a whole is never skip-only.

A future release bump is a new source and fault audit, not a semver
substitution.
