# Issue #671 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Database filesystem: `/dev/md1` ext4

Baseline attribution commit: `ea253dc9c4f51471a9b1bf9d9c563cbe44bf6ab3`

Candidate commit: `0ff2d2bac2609a9e4fd8a0cc002e7b5747deb985`

The following commit reverts only the production candidate. Raw reports retain
the exact baseline and candidate commit identities.

Shape source SHA-256:
`83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72`

Candidate binary SHA-256:
`5b2358c2d2b8dd7c705381c8b6697f0d54f6f7cfb9f3f06de100d9affeab8276`

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
3 fresh-process reports. Redb databases were retained through exact reopen and
then removed; only immutable JSON reports are committed.

## Verification

Before the ceiling runs, focused verifier tests passed 6 tests with the
existing opt-in real-corpus test ignored. After reverting the production
candidate, the following verification passed:

- `cargo test -p nmp-transport`: 96 tests passed; the existing opt-in
  real-corpus test remained ignored.
- `cargo clippy -p nmp-transport --all-targets --all-features -- -D warnings`
- `cargo check -p nmp-engine --features bench-instrumentation --example relay_ingest_bench`
- `cargo fmt --all -- --check`
- `git diff --check`
