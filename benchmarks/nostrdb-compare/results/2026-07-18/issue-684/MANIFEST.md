# Issue #684 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Probe schema: `nmp-relay-ingest-probe-v21`

Baseline commit: `337356e6da0923cc8fe536df5c434c1dbb91798c`

Candidate source-diff SHA-256:
`f8b83bd2cd1689753a58d53a736c666920856c65d73ed83593b30bbe9f8e4820`

Baseline release probe SHA-256:
`37c945e3fb4fea7d04cd5e595b146425647b7f92680c8911c7333ddec9ee3f1d`

Candidate release probe SHA-256:
`a572600d4e24c09a21a84ea297e89afaf9043d3b9a224847e50372218432c4b6`

Shape-source BLAKE3:
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Result sets

- `profile/baseline/run.json`: baseline run captured under perf.
- `profile/candidate/run.json`: retained candidate captured under perf.
- `profile/*/perf-header.txt`: exact perf metadata and command.
- `profile/*/engine-thread.perf-script.txt.gz`: raw symbolized sample stream
  for the engine thread selected from each perf capture. The complete
  `perf.data` files were 208 MiB and 241 MiB and are intentionally not added
  to git; the committed streams preserve every selected-thread sample and
  callchain used by the finding.
- `paired/`: five alternating-order fresh-process baseline/candidate pairs
  used for the decision. Order was B/C, C/B, B/C, C/B, B/C.

The candidate binary was built from the recorded uncommitted diff over the
baseline commit. Both probe JSONs therefore honestly embed the common base
commit; the source-diff and binary digests above identify the candidate bytes.

## Profile command

`perf_event_paranoid=3` required `sudo` on this host.

```sh
sudo perf record \
  -e cycles:u \
  -F 999 \
  -g \
  --call-graph dwarf,16384 \
  -o PROFILE.perf.data \
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
  --output PROFILE.json
```

## Paired benchmark command

The same probe arguments above were used without `perf` and with a fresh
output path for each process.

## Verification

- `cargo test -p nmp-engine coverage_evidence_refresh_tests --lib`: passed.
- `CARGO_INCREMENTAL=0 cargo test -p nmp-engine`: passed (all non-environmental
  tests; the existing real-corpus test remained ignored).
- `cargo clippy -p nmp-engine --all-targets --all-features -- -D warnings`:
  passed.
- `cargo fmt --check`: passed.
- `scripts/check-sdk-parity.sh`: passed.
- `git diff --check`: passed.
- Architecture gates 1-4: no new public noun, error variant, lifecycle bool,
  or destructive API.
