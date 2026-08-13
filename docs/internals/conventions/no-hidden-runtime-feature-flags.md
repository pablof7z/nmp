---
title: No hidden runtime feature flags
category: conventions
slug: no-hidden-runtime-feature-flags
status: policy
date: 2026-08-13
owns:
  - requested behavior runs on the normal runtime path
  - runtime gates require an explicit product decision
related:
  - docs/internals/conventions/no-backwards-compatibility.md
issues:
  - https://github.com/pablof7z/nmp/issues/1420
---

# No hidden runtime feature flags

Requested behavior runs on the normal path. Never hide it behind `ENABLE_X=1`,
an environment variable, config boolean, rollout/experimental switch, or
undocumented opt-in.

If it is not ready to be active, it is not ready to merge. Tests use the normal
path.

Allowed:

- Rust/Cargo features;
- real configuration: relays, providers, credentials, endpoints, requested
  modes;
- staged rollout, kill switch, or optional behavior explicitly required by the
  user or owning product/design decision.

Never invent a gate to hedge risk, preserve old behavior, ease merging, or skip
caller/test updates.
