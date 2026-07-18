# Issue #679 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Probe schema: `nmp-relay-ingest-probe-v21`

Attribution baseline tree: `4847155278767e574fa297c3d38e3aeb72c36bfd`

Retained production candidate tree: `66091e9ee98773b81c3ec651bab9d6bf4b598b59`

Candidate release probe SHA-256:
`37c945e3fb4fea7d04cd5e595b146425647b7f92680c8911c7333ddec9ee3f1d`

Shape-source BLAKE3:
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Result sets

- `baseline/`: 3-process schema-v20 resolver/engine CPU reconciliation.
- `memory-subphases/`: corrected 3-process schema-v21 MemoryStore attribution.
- `compact-event-keys/`: 3-process event-key-only intermediate candidate.
- `compact-event-author-keys/`: 3-process complete candidate precheck.
- `paired/`: 5 alternating-order fresh-process baseline/candidate pairs used
  for the decision.

The paired binaries were built from these exact trees. Tree ids are used
instead of commit ids because this branch was rebased over the prerequisite
#676 evidence PR without changing either measured source tree.
The embedded `git_commit` therefore names the common attribution commit while
the binary digest above and candidate commit identify the retained source.

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
  --memory-store \
  --output FRESH.json
```

## Verification

- `cargo test -p nmp-store`: passed.
- `cargo test -p nmp-resolver`: passed.
- `CARGO_INCREMENTAL=0 cargo test -p nmp-engine`: passed.
- Architecture gates and clippy are recorded in the PR checks.
