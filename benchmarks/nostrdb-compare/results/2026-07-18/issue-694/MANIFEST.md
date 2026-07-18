# Issue #694 result manifest

Date: 2026-07-18

Candidate commit: `1073526e3279708e381a360b7fd01813d569a4ab`

Host: `kind2-linux-x86_64`

Probe schema: `nmp-relay-ingest-probe-v21`

Release probe SHA-256:
`b7e0d0c9150d03d2e8fe0eb35b41b11e2188fdf0a07f8db69dc3a1c9789488e6`

## Corpus and fixed configuration

- shape source:
  `benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json`
- shape-source BLAKE3:
  `d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`
- generated events per process: `100,000`
- backend/durability: Redb / Immediate
- queue capacity: `8,192`
- verifier workers / batch: `8` / `512`
- engine byte ceiling: `8 MiB`
- engine coalescing wait: `200 us`
- visible window: `200`

## Build

```sh
cargo build --release -p nmp-engine \
  --features bench-instrumentation \
  --example relay_ingest_bench
```

## Stage-1 matrix

Each child used the same command shape, changing only
`--engine-batch-size` between `4096`, `8192`, and `16384`:

```sh
target/release/examples/relay_ingest_bench \
  --events 100000 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --committed-observation-cache-capacity 131072 \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size COHORT \
  --engine-batch-bytes 8388608 \
  --engine-batch-wait-us 200 \
  --visible-limit 200 \
  --timeout-secs 120 \
  --output RESULT.json
```

Five triplets rotated the cohort order. Every run observed exactly `100,000`
relay EVENT frames, ended with exactly `200` visible rows, and reopened the
durable store successfully.

The `16,384` configuration never formed more than `8,704` events in one
bridge/store batch; its observed maximum encoded batch was about `4.5 MiB`.
The current producer/queue/`200 us` envelope, not the `8 MiB` ceiling, limited
that setting.

## Stop boundary

The `8,192` candidate missed the `10%` throughput gate and failed the `10%`
RSS gate. The `16,384` setting failed the RSS gate and did not exercise its
configured count ceiling. Per #694, MemoryStore, replay, one-million, and
production-default stages were not run after stage 1 failed.

No runtime code changed. Final-tree verification is:

```sh
git diff --check master...HEAD
scripts/check-sdk-parity.sh
scripts/check-falsifier-honesty.sh master HEAD
```

## Raw-result aggregate SHA-256

The digest below hashes the sorted set of the 15 individual JSON content
digests, independent of file path:

```text
b46d3b9b442f9f0a6a8cbe2dcd4a16cc6fd0ffceaf062964b41e40e8ffb15681
```
