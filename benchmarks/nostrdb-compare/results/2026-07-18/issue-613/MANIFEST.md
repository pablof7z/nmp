# Issue #613 evidence manifest

## Revisions

- clean baseline: `ecfcf410b8a47e3f2ada60aa8120064703feb258`
- instrumented old ownership path: `132bb9944794154d27badc5a023ec3a4d15c918c`
- borrowed/move ownership candidate: `868d8f63181c02c893ae8879e0e6bec2e1400667`
- candidate reversion: `1a7f50b`

The candidate and baseline executables were built separately in release mode
without `bench-instrumentation` for the decision matrices. The instrumented
pair was built with `--features bench-instrumentation` and is used only for
clone/allocation attribution.

## Production command

```sh
relay_ingest_bench \
  --events 100000 \
  --queue-capacity 8192 \
  --verified-cache-capacity 131072 \
  --verifier-workers 8 \
  --verify-batch-size 512 \
  --engine-batch-size 4096 \
  --engine-batch-bytes 8388608 \
  --timeout-secs 240 \
  --shape-corpus benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  --store FRESH.redb \
  --output FRESH.json
```

The MemoryStore control used the same command with `--memory-store` instead of
`--store`.

Each matrix contains ten alternating pairs. Odd pairs ran baseline first;
even pairs ran candidate first. Every child was a fresh process and every
Redb child used a fresh store. No clone/allocation atomics were compiled into
these executables.

## Raw evidence

- `redb/`: 20 clean Redb production reports.
- `memory/`: 20 clean MemoryStore production reports.
- `instrumented/baseline.json`: old ownership path with exact clone/allocation
  counters.
- `instrumented/candidate.json`: borrowed/move candidate with exact
  clone/allocation counters.

The reports contain the host, compiler, kernel, CPU, corpus hash, configuration,
revision, throughput, latency, memory, write, reopen, and exact-count fields.

