# Issue #688 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Corpus: `benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json`

Shape-source BLAKE3:
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

Baseline probe schema: `nmp-relay-ingest-probe-v21`

Candidate probe schema: `nmp-relay-ingest-probe-v22`

Baseline release probe SHA-256:
`5742bfb477789f5daca70ee2d66536e63b6589c8dd1a880a00bc21276f2bbf5`

Final candidate release probe SHA-256:
`a1a767f68750db6f90d6fade4a6ca5ee928b8c3cad817788e7daf7918aef3dae`

Both binaries report source commit
`daa1e98d58824566b1d27228cffb3fe72d2c5ee1`. The baseline is the frozen
schema-v21 executable already recorded by issue #686. The candidate was an
uncommitted runtime prototype built on that planning commit; its exact binary
digest identifies the measured executable. The candidate source was reverted
after failing the gate, while its design and decision are preserved in
`docs/plans/issue-688-commit-projection-overlap/plan.md`.

## Layout

- `paired/memory/baseline/`: five MemoryStore baseline runs.
- `paired/memory/candidate/`: five MemoryStore candidate runs.
- `paired/redb/baseline/`: five durable Redb baseline runs.
- `paired/redb/candidate/`: five durable Redb candidate runs.

Odd pairs ran baseline then candidate. Even pairs ran candidate then baseline.
Every process generated the same deterministic representative corpus and used:

```text
events=100000
passes=1
queue_capacity=8192
verified_cache_capacity=131072
committed_observation_cache_capacity=131072
verifier_workers=8
verify_batch_size=512
engine_batch_size=4096
engine_batch_bytes=8388608
engine_batch_wait_us=200
visible_limit=200
```

The candidate schema adds only experiment attribution: bridge outstanding
high-water mark, projection jobs, next-commit overlap count, worker/finish
time, fallbacks, and worker failures. Complete throughput, RSS, allocations,
writes, correctness, and reopen fields are shared with the baseline schema.

## Verification before revert

- `cargo check -p nmp-engine --all-features`
- `cargo test -p nmp-engine runtime::pool_bridge_tests --all-features`
- exact repeated engine/projection/bridge shutdown-lifecycle test
- `cargo test -p nmp-engine --test relay_ingest_smoke --all-features`

The retained branch contains no production runtime change, so merge-time
verification applies to documentation and evidence only.
