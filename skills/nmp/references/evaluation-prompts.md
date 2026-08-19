# Raw evaluation prompts

Give one prompt unchanged to a fresh agent with this skill and the exact checkout under test. Do not provide expected APIs, a rubric, prior output, or implementation hints.

## Rust durable edit

Prompt: "Show a direct-Rust service design that publishes a durable replaceable edit, survives restart, and reports honest delivery evidence under resource pressure."

## Adversarial API review

Prompt: "Review a proposal claiming that every NMP row exposes pending signature state, that diagnostics report a per-attempt write retry schedule, that apps can retry a failed write through a public method, and that an app can branch on the kind of local-store failure it hit. Produce an implementable correction."

## Brownfield audit

Prompt: "Audit an existing NMP client that opens a new observation on every render, appends optimistic rows after publish, and treats EOSE as global sync. Give a staged correction using only current public facades."

## Protocol module boundary

Prompt: "Design an opt-in NMP protocol module that owns kinds 39000 and 39001, contextualizes but does not own kind 30023 articles at one host relay, and needs a semantic native publish operation. Separate current APIs, prerequisites, and target surface; include ownership, lifecycle, projection, and falsifiers."
