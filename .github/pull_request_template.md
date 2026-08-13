## Why / failure evidence

- Issue:
- Consequence or falsifier that requires this change:

## Verification

- [ ] Tests for every touched crate pass.
- [ ] Running/result-level proof is recorded where compilation alone is insufficient.
- [ ] New falsifiers fail against the old behavior and pass here.

## Public-surface impact

- [ ] Rust / FFI / Swift / Kotlin impact is described, including “none.”
- [ ] Superseded path removed: the obsolete API/semantic path is deleted in the same PR; no compatibility alias remains.
