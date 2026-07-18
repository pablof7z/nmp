# Issue #675 — combined shifted-bottleneck ceiling

## Decision

The current optimization portfolio cannot meet #612's 150,000 events/s gate,
even when its individually rejected stages are combined as unsafe favorable
ceilings.

The strongest complete MemoryStore ceiling reached 135,123 events/s median,
51.1% above the exact #673 baseline but still 9.9% below the epic gate. An
earlier independent full-ceiling repetition reached 126,633 events/s median
and is also retained. Every run observed exactly 100,000 frames and ended with
exactly 200 visible rows.

The benchmark combination made these costs artificially favorable:

- EVENT JSON parsing was replaced by preloaded owned events.
- Relay-provided event IDs and signatures were trusted.
- Signature tasks/results were batched by verifier lane.
- Bounded history candidates were ranked before transient row cloning.

None of those relaxations ships. The history and verifier candidates were
reverted after measurement, and ordinary builds cannot enable the validation
or parse bypasses.

## Staged MemoryStore matrix

| Mode | Median throughput | Change from exact baseline | Median wall |
| --- | ---: | ---: | ---: |
| Exact #673 baseline | 89,432 events/s | — | 1,118.2 ms |
| Bounded history candidate | 87,007 events/s | 2.7% lower | 1,149.3 ms |
| History + verifier lanes | 84,383 events/s | 5.6% lower | 1,185.1 ms |
| History + lanes + free ID/signature validation | 106,961 events/s | 19.6% higher | 934.9 ms |
| Full ceiling, first matrix | 126,633 events/s | 41.6% higher | 789.7 ms |
| Full ceiling, attributed repeat | 135,123 events/s | 51.1% higher | 740.1 ms |

Values are independent medians of 3 fresh processes. The exact baseline is
the committed #673 matrix on the same host, corpus, binary settings, and
MemoryStore path. The two full-ceiling matrices use the same candidate; the
second adds benchmark counters and confirms the decision despite normal host
variance.

The interaction is real. Free validation was flat in #673, while the same
ceiling became useful after history and verifier work changed the producer and
consumer balance. Independent 10% gates would have hidden that. They remain
useful for optional complexity, but a set of jointly necessary pipeline
changes must be judged by its final exact combination.

## Remaining constraint

In the strongest attributed matrix, effect dispatch accumulated only 1.1 ms.
The engine reducer itself accumulated 1,019.8 ms across batches. Its median
known components were:

- Resolver: 545.0 ms.
  - semantic store: 247.9 ms;
  - event classification: 174.6 ms;
  - preparation: 111.5 ms;
  - reaction/affected-set work: 11.4 ms.
- Committed apply, including the reduced history projection: 52.9 ms.
- Post-store publication/diagnostic preparation: 9.5 ms.
- Relay-ingest prelude: 2.1 ms.

That leaves about 410 ms inside ordinary relay-frame reduction before the
measured ingest phases. The code in that interval validates the current
session, moves typed frames into candidates, constructs provenance, and bumps
the nested per-session/per-kind diagnostics map once per event. It is now the
largest unattributed owner, followed by resolver classification and semantic
MemoryStore work.

Transport `delivery_ns` in this ceiling is backpressure: the artificially fast
producer blocks on the bounded engine queue. It is not independent work to
optimize.

## Consequence for #612

Do not implement a custom parser, alternate Schnorr library, lane batching, or
the history candidate as isolated follow-ups. Even deleting all of their
measured cost together misses the target.

The next issue should split and batch the ordinary `on_relay_frames` planning
interval, especially diagnostics counting and repeated session/provenance
work. A production candidate is justified only if it moves the complete exact
pipeline; after that, resolver classification/store work remains the next
measured owner.
