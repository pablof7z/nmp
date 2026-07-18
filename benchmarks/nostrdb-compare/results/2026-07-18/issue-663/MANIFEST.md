# Issue #663 evidence manifest

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
