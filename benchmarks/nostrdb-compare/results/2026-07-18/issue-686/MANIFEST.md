# Issue #686 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Probe schema: `nmp-relay-ingest-probe-v21`

Profile and baseline commit:
`a0129a040ebca89c50d54187e729ada08ccb8838`

Baseline release probe SHA-256:
`5742bfb477789f5daca70ee2d66536e63b6589c8dd1a880a00bc21276f2bbf5`

Rejected candidate release probe SHA-256:
`16d800b603218afe122249b86fe0bfeed05ec519ef0ee372b2e4b17d1b961a95`

Shape-source BLAKE3:
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Exact active-completion profile

`profile/active-completion/window.json` records the
`CLOCK_MONOTONIC_RAW` start and end bounds captured by the benchmark itself.
`all-threads.perf-script.txt.gz` is the raw symbolized `perf script` stream
filtered to those bounds. `perf-header.txt` preserves the complete capture
metadata and command. The 195 MB binary perf recording is omitted because the
filtered symbolized stream is the reviewable evidence used for attribution.

The profile command was:

```sh
sudo perf record \
  -e cycles:u \
  -F 999 \
  -g \
  --call-graph dwarf,16384 \
  -o /tmp/nmp686raw.perf.data \
  -- relay_ingest_bench \
  --events 100000 \
  --passes 1 \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --committed-observation-cache-capacity 131072 \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size 4096 \
  --engine-batch-bytes 8388608 \
  --engine-batch-wait-us 200 \
  --timeout-secs 300 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --memory-store \
  --completion-window-output WINDOW.json \
  --output RUN.json
```

The reviewable stream was produced with:

```sh
sudo perf script \
  -f \
  -i /tmp/nmp686raw.perf.data \
  --time 8043855.050336960,8043856.219156053
```

## Paired decision sets

- `paired/memory/`: 5 alternating-order fresh-process MemoryStore pairs.
- `paired/redb/`: 5 alternating-order fresh-process durable Redb pairs.
- `baseline/` and `candidate/` contain the corresponding run from each pair.

The rejected candidate reapplied only the borrowed-row ranking and early
bounded-window truncation from `1b42e3073b941df607d1697f7dea27c3743294e3`
to `history_lifecycle.rs` atop the profile commit. Existing schema-v21
attribution remained unchanged. The candidate binary digest above identifies
the exact measured executable; the production hunk is absent from the final
branch.

## Verification

- Focused bounded-history tests: passed for the candidate before measurement.
- `CARGO_INCREMENTAL=0 cargo test -p nmp-engine`: passed.
- Architecture gates, formatting, and clippy are recorded in the PR checks.
