# Issue #661 evidence manifest

## Revision

- probe implementation and measured binary:
  `58619fffb858db54d71ec856f3ed1402b613fa2f`
- report schema: `nmp-relay-ingest-probe-v10`

The executable was built in release mode without `bench-instrumentation`.

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

The MemoryStore control replaced `--store FRESH.redb` with `--memory-store`.
Odd pairs ran Redb first; the even pair ran MemoryStore first. Every Redb run
used a fresh store, and every report verified the expected visible projection.
Redb reports additionally reopened exactly 100,000 canonical events.

The duplicate replay used the same Redb command with `--passes 2` and a
300-second timeout. Its replay clock starts at the first offered frame of the
second pass and ends when all 200,000 expected frames and the visible projection
are observed; the subsequent quiet proof remains outside that clock.

## Raw reports

- `redb/`: three fresh-process Redb reports.
- `memory/`: three fresh-process MemoryStore reports.
- `replay.json`: one fresh-process two-pass Redb replay report.

Each report records the host, compiler, kernel, CPU, corpus hash, complete
configuration, both timing intervals, throughput, latency, memory, physical
writes, and exact reopen result.
