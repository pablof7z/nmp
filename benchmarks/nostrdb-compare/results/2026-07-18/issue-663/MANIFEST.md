# Issue #663 evidence manifest

## Production revision

- measured implementation: `daccf5ab4bc770b9396b1b5c9ea486ebd0e29cfc`
- release binary SHA-256:
  `117e7ea7ecb0021854e234656919c6bd981378654c25fed9a5ae444c4945f9e3`
- report schema: `nmp-relay-ingest-probe-v12`
- corpus source hash: `d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Production replay command

```sh
relay_ingest_bench \
  --events 100000 \
  --passes 2 \
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

Three fresh processes and fresh Redb files produced `production/run1.json`
through `production/run3.json`. No diagnostic ceiling flag was enabled. Every
report embeds the exact measured commit above and observes 200,000 EVENT
frames, 100,000 committed-observation hits, 100,001 parsed frames including
EOSE, and 100,000 resolver/store events.

## Ordinary-ingest control

The production command was repeated with `--passes 1` in three fresh
processes each at `--engine-batch-wait-us 0` and `200`. The raw reports are in
`ordinary-control/`. The medians were 36,971 and 41,330 events/s respectively.

## One-million scale and reopen

The exact production binary was copied before build-artifact cleanup and run
once with `--events 1000000`, `--passes 1`, the same production settings, and
a 1,200-second deadline. `scale/one-million.json` observed and reopened exactly
1,000,000 events with the default globally bounded 131,072-entry committed
cache. This is a scale, memory, and recovery falsifier, not a repeated
throughput estimate.

## Production raw reports

- `production/run1.json`
- `production/run2.json`
- `production/run3.json`
- `ordinary-control/wait0-run1.json` through `wait0-run3.json`
- `ordinary-control/wait200-run1.json` through `wait200-run3.json`
- `scale/one-million.json`

## Favorable ceiling revision

## Revision

- measured implementation: `e18dd1f27980d2e85bab898fa10e66df461dcf31`
- report schema: `nmp-relay-ingest-probe-v11`
- corpus source hash: `d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Command

```sh
target/release/examples/relay_ingest_bench \
  --events 100000 \
  --passes 2 \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --diagnostic-duplicate-ceiling-capacity 131072 \
  --diagnostic-duplicate-ceiling-event-payload \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size 4096 \
  --engine-batch-bytes 8388608 \
  --engine-batch-wait-us 50 \
  --timeout-secs 300 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --store FRESH.redb \
  --output FRESH.json
```

The release binary was built with `--features bench-instrumentation`. Each run
used a fresh process and fresh Redb file on the same local ext4 device. The
probe regenerated the deterministic private-free representative corpus before
the timed path, verified the visible projection and diagnostics, shut down,
and reopened the store exactly.

## Raw reports

- `ceiling/run1.json`
- `ceiling/run2.json`
- `ceiling/run3.json`
