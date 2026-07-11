## Why / failure evidence

- Issue:
- Consequence or falsifier that requires this change:

## Verification

- [ ] Tests for every touched crate pass.
- [ ] Running/result-level proof is recorded where compilation alone is insufficient.
- [ ] New falsifiers fail against the old behavior and pass here.

## Public-surface protocol

- [ ] **Failure evidence:** the issue/PR explains what breaks or remains unproved.
- [ ] **Changed projections:** the exact governed set (`ffi,kotlin,rust,swift`, or `correction`) matches the CI context.
- [ ] **Rust / FFI / Swift / Kotlin impact:** every projection is assessed and synchronized tests are named.
- [ ] **Persistence impact:** store/journal/key compatibility is described, including “none.”
- [ ] **Diagnostics impact:** evidence/coverage/receipt consequences are described, including “none.”
- [ ] **Updated falsifiers:** concrete tests are linked or listed.
- [ ] **Superseded path removed:** the obsolete API/semantic path is deleted; no compatibility alias remains.
- [ ] **Surface snapshots:** `scripts/regenerate-surface-snapshots.sh` was run and any diff is committed.
- [ ] **Append-only trail:** if a snapshot changed, a complete entry was appended to `docs/surface-change-log.md`; old entries were not edited.
- [ ] **Human signoff:** reviewer approval covers the snapshot delta and log entry.
