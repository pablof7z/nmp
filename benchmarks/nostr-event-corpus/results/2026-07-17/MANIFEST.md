# Reproduction manifest

- Issue: #620 (parent #612)
- Base commit: `236c300299b63fed7649fa4ce79c04baabcb7708`
- Rust: `rustc 1.99.0-nightly (da80ed070 2026-07-14)`
- Capture schedule: `capture-2026-07-10.sh`
- Accepted source hash (BLAKE3): `d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`
- Raw redistribution: no; rerun the checked acquisition script and compare the
  source hash. Public relay retention can change, so the committed distribution
  and private-free shapes are the durable evidence.
- Representative production configuration: 100,000 events, queue 8,192,
  verified cache 131,072, 8 verifier workers, verify batch 512, engine batch
  4,096, engine byte bound 8 MiB, 240-second timeout.
- Matrix order: uniform then representative, repeated three times in fresh
  processes by `run-production-matrix.sh`.
- Scale configuration: the same parameters with 1,000,000 events and a
  600-second timeout; duplicate replay uses 100,000 events, two passes, and a
  300-second timeout.

## SHA-256

```text
0be7a8d5868ccf38433e66077165b2dc71f4a6206b58cd61f65cbecc0ffcca16  distribution.json
83f6fe1ec2947471b4754f42749ca5df5525bf8490fb8c67286b78a7bf55de72  private-free-shapes.json
9844f617684f18a8811095016b4cbb5a04f5f903828ac3a08678820144c306f1  privacy-string-inventory.txt
bb9c162935a9ff2781c2eb99be2a6a65fa9ae80af5bee6af5c4e530315b13786  production-matrix/representative-1.json
35641e00eee6e2a3bd474510b89c9ec4a90df3cb9ccd91abebd54fb279a133b2  production-matrix/representative-2.json
9c04573201410fa5d8e83f97b6f08610ad19abb326f977c407700de3ebd62753  production-matrix/representative-3.json
43cfc3a8a149df2fa63bec3b7c12127ac790bd9ce7efe0d1dd03d2b46ccf81f0  production-matrix/uniform-1.json
9e72551ed2cef724b849c0f6fe31acf34384024fe6a719cafaee29801374004c  production-matrix/uniform-2.json
75c11f6daf2fa7b28a86ef568fd03a5b30f73f82ba5d519fd185b47915e2858f  production-matrix/uniform-3.json
c6e44281bc245723b63a66172b7b49e822b5ff5d0e7e4ab69cca0ccacfaa9c8a  scale/representative-100k-duplicate.json
b577e692cc2a3284d3f2452fdb82b9865b3220ea81180dd8e3b84c00cb4fe7ff  scale/representative-1m.json
```
