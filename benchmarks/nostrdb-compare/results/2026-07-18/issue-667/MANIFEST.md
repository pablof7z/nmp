# Issue #667 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Database filesystem: `/dev/md1` ext4

Baseline source commit: `3b7e1c5d2b2cb706deeaedd14985a94f3a1d2026`

Candidate commit: `1b42e3073b941df607d1697f7dea27c3743294e3`

The candidate commit contains both benchmark-only attribution and the bounded
materialization ceiling. The following commit reverts only the production
candidate; the raw reports retain the exact candidate commit identity.

Shape source SHA-256:
`83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72`

Candidate binary SHA-256:
`14cb30d73c0f09aefebd6e008350035e1ee91fef56d98854d5470be67afdbc51`

## Command

```sh
relay_ingest_bench \
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
  --store FRESH.redb \
  --output FRESH.json
```

MemoryStore runs added `--memory-store` and omitted `--store`. Each matrix has
3 fresh-process reports. Redb databases were retained through each child's
exact reopen check and then removed; only immutable JSON reports are committed.

## Verification

```sh
cargo test -p nmp-engine
cargo clippy -p nmp-engine --all-targets --all-features -- -D warnings
cargo fmt --all -- --check
git diff --check
```

All passed after the candidate revert. The engine library reported 166 tests;
the integration suites also passed, with only the existing opt-in real-corpus
test ignored. The focused history mutation suite passed all 10 tests before
the ceiling runs.
