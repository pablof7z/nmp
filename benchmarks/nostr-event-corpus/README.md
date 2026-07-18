# Public Nostr event corpus harness (#620)

This isolated benchmark crate measures exact public relay `EVENT` frames and
derives a privacy-safe workload description for NMP's tiny-ingest benchmarks.
It is outside the production workspace and dependency graph.

The checked capture schedule samples the same two-second UTC interval every
twenty minutes during 2026-07-10 through 2026-07-16 across six independently
operated general-purpose public relays. This is systematic temporal sampling,
not a claim that the relays are a random sample of the entire Nostr network.
The result records relay and time-window bias explicitly.

Two seconds is deliberate. Rejected one-minute and ten-second captures
repeatedly returned exactly 500 frames from several relays and exactly 100 from
another, revealing silent relay response caps. Two-second windows stay below
those observed ceilings, while the twenty-minute stride keeps the total sampled
duration comparable and triples temporal coverage.

The relay-selection pilot also rejected `relay.nostr.net`: it still returned
exactly its hidden 100-frame ceiling in 3 of 504 two-second windows. Two other
attempted relays (`nostr.bitcoiner.social` and `relay.snort.social`) returned no
events for the historical windows. None of those three contributes to the
reported distribution.

## Reproduce

```sh
cargo build --release --manifest-path benchmarks/nostr-event-corpus/Cargo.toml
benchmarks/nostr-event-corpus/capture-2026-07-10.sh \
  benchmarks/nostr-event-corpus/target/release/nmp-nostr-event-corpus \
  /tmp/nmp-620-captures
benchmarks/nostr-event-corpus/target/release/nmp-nostr-event-corpus analyze \
  /tmp/nmp-620-captures 10000 \
  /tmp/nmp-620-distribution.json \
  /tmp/nmp-620-private-free-shapes.json
```

Each capture manifest binds the relay URL, exact half-open windows, raw frame
counts and bytes, per-window BLAKE3, and whether the requested 5,000-frame
ceiling was reached. Analysis re-hashes every source file, verifies every event
id and signature, reports malformed/invalid/conflicting rows, and reports both
observation-level and unique-event distributions.

The raw frames remain outside git because they contain public users' content,
pubkeys, signatures, and encrypted payloads. The committed shape corpus keeps
only kind numbers, byte and JSON-encoding costs, public protocol tag-name
classes, and coarse tag-value classes. It contains no ids, pubkeys, signatures,
content, or tag values.

## Production benchmark

Build the benchmark from the repository root, then alternate the historical
uniform workload with the representative workload:

```sh
cargo build --release -p nmp-engine --example relay_ingest_bench \
  --features bench-instrumentation
benchmarks/nostr-event-corpus/run-production-matrix.sh \
  target/release/examples/relay_ingest_bench \
  benchmarks/nostr-event-corpus/results/2026-07-17/private-free-shapes.json \
  /tmp/nmp-620-production-matrix
```

The checked [result summary](results/2026-07-17/SUMMARY.md) records the capture
bias, complete distribution, alternating 100k matrix, exact one-million run,
and duplicate replay. [The manifest](results/2026-07-17/MANIFEST.md) binds the
commands, source hash, configuration, and raw result hashes.

Probe schema v10 separates active completion from its correctness quiet proof.
`completion_ingest_ms` stops the first time every expected relay frame and the
required visible projection are observed;
`completion_relay_frames_per_second` is the governing production-throughput
metric. `observation_and_quiet_ms` includes the subsequent quiet confirmation
and final receiver drain. Keep that wider duration for correctness and resource
accounting, never as the throughput denominator. Runs with more than one pass
also report `replay_completion_ms` and `replay_frames_per_second`, measured
from the first offered frame after the initial pass through final observation;
that is the duplicate-replay gate rather than the aggregate initial-plus-replay
rate.
