# Issue #673 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Evidence source commit after rebasing onto merged #672:
`add81dcdd6505f5b0498c2ea8ce705d528637cfd`

Evidence source tree:
`b92ca8c1bd3d4c69ccf9d9a74f7269ad19bdf065`

Raw reports record the pre-rebase commit
`76bd056253e76c1ff78ed9bd3fc65870fd80ff67`; its source tree is exactly the
same `b92ca8c1bd3d4c69ccf9d9a74f7269ad19bdf065` tree above.

Shape source SHA-256:
`83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72`

Release probe SHA-256:
`6ec829b5d0f024271c8bcbd49054d74bd45cedc333bd2547525bf0698b24ac99`

Probe schema: `nmp-relay-ingest-probe-v17`

## Base command

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
  --memory-store \
  --output FRESH.json
```

The validation matrices added one of:

- `--diagnostic-skip-event-id-validation`
- `--diagnostic-skip-signature-verification`
- both validation flags

The parse ceiling added `--diagnostic-preparsed-ceiling`. That mode is
restricted to one relay and one pass. It loads the exact generated owned
events before the measured ingest interval, then consumes them once in wire
order. It is an intentionally unsafe favorable ceiling, not a parser
candidate. Its retained preload makes absolute RSS, rather than RSS growth,
the honest memory comparison.

Each directory contains 3 fresh-process reports. Every report records exactly
100,000 observed frames and 200 final visible rows.

## Verification

Before the evidence run:

- `cargo test -p nmp-transport --lib`: 91 passed; the existing opt-in real
  corpus test remained ignored.
- Instrumented and ordinary example checks passed.

After the diagnostic candidate was frozen:

- `cargo test -p nmp-transport`: 96 tests passed; the existing opt-in real
  corpus test remained ignored.
- `cargo clippy -p nmp-transport --all-targets --all-features -- -D warnings`
- Instrumented and ordinary `nmp-engine` example checks.
- `cargo fmt --all -- --check`
- `git diff --check`
