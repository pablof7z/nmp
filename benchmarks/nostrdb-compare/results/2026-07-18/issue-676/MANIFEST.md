# Issue #676 result manifest

Date: 2026-07-18

Host: `kind2-linux-x86_64`

Probe schema: `nmp-relay-ingest-probe-v19`

Final reverted baseline commit:
`a72d69742660c81c7100e9770c1b0d5bddd3bebe`

Final source tree:
`d12afb5637ee93176a02425d190bad962d7967c6`

Release probe SHA-256:
`bd832ec79af98031c26b4452fa5e25a2f9f50c75bc87284fdeb5abf47dc591ad`

Shape source BLAKE3:
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`

## Candidate commits

- Attributed comparable baseline: `c31c6d1c0de25a489a1dcc293145f69c97f754e0`.
- Reuse validated session: `64df77e7ccada994c89b6b2754bbc0270a2a9b4f`.
- Shared pool/engine session plus batched diagnostics:
  `426764b1d761cd8a652c3ee28dd08d94037661e9`.
- `d4d176a` and `a72d697` revert both production candidates.

`attributed-baseline/`, `session-reuse/`, and `shared-session-batches/`
are the directly comparable 3-process throughput matrix. `final-baseline/`
adds engine-thread CPU attribution after both production candidates were
reverted.

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

- `cargo test -p nmp-engine`: passed after both candidates were reverted.
- `cargo test -p nmp-transport`: passed after both candidates were reverted.
- `cargo clippy -p nmp-engine -p nmp-transport --all-targets --all-features
  -- -D warnings`: passed.
- `cargo fmt --all -- --check`: passed.
- `git diff --check`: passed.
