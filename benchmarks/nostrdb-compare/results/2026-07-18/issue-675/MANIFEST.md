# Issue #675 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Full attributed ceiling commit:
`d67758256497709f92b9455516965fe66b4ae0e1`

Full attributed source tree:
`9a98064609e9d7e4a16e692f78e189750ee42d6a`

Probe schema: `nmp-relay-ingest-probe-v18`

Release probe SHA-256:
`98f8281154f638ec16854b1f258d8e9c6314e52beda1f8358225e853c3aede44`

Shape source SHA-256:
`83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72`

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

The free-validation stage added:

```sh
--diagnostic-skip-event-id-validation \
--diagnostic-skip-signature-verification
```

The full ceiling additionally added:

```sh
--diagnostic-preparsed-ceiling
```

The production-shaped history candidate was commit `daed738`; verifier lane
batching was layered at `486a3e7`. Attribution was layered at `d677582`.
Commits `3f09f03` and `844e279` revert the two production candidates after the
ceiling run. The benchmark-only attribution remains.

Every directory contains 3 fresh-process reports. `full-ceiling-attributed/`
is the deciding schema-v18 repeat. `full-ceiling/` is the earlier independent
schema-v17 repetition. The exact baseline is committed under issue #673.

## Verification

- Focused history tests: 15 passed.
- Focused verifier tests: 6 passed; the existing opt-in real-corpus test
  remained ignored.
- Instrumented example check passed after adding the final attribution.

- `cargo test -p nmp-engine`: passed after both production candidates were
  reverted.
- `cargo clippy -p nmp-engine --all-targets --all-features -- -D warnings`:
  passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
