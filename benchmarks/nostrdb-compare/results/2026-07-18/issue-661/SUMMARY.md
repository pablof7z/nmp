# Issue #661 — honest ingest-completion throughput

## Decision

Probe schema v9 divided expected relay frames by a duration that included the
one-second accepted-quiet proof and the final 100 ms receiver drain. On the
epic's 100,000-event corpus, that denominator could never report the 150,000
events/s completion gate even if active ingest were instantaneous.

Schema v10 removes the misleading fields and reports two distinct clocks:

- `completion_ingest_ms` stops the first time all expected relay frames and
  the required visible projection are observed.
- `observation_and_quiet_ms` retains the wider correctness and resource
  accounting interval.
- `completion_relay_frames_per_second` uses only the completion duration.
- Two-pass runs additionally report `replay_completion_ms` and
  `replay_frames_per_second` from the first offered replay frame, so the
  duplicate gate no longer uses an initial-plus-replay aggregate.

The quiet proof remains mandatory. It is no longer mislabeled as ingest work.

## Fresh production baseline

Three fresh-process pairs alternated Redb and MemoryStore on the representative
100,000-event corpus. Medians:

| Metric | Redb | MemoryStore |
| --- | ---: | ---: |
| completion throughput | 42,234 events/s | 91,867 events/s |
| completion wall | 2,367.8 ms | 1,088.5 ms |
| observation plus quiet | 3,469.2 ms | 2,189.4 ms |
| v9-equivalent reported throughput | 28,825 events/s | 45,674 events/s |
| last-row wall | 2,367.7 ms | 1,088.5 ms |
| peak RSS | 122.9 MB | 643.3 MB |

The corrected metric is 46.5% higher for Redb and 101.1% higher for
MemoryStore. This is a measurement correction, not a production speedup. In
every initial-ingest run, captured completion and last-row delivery differed by
less than 0.1 ms, while the wider interval retained about 1.1 seconds of quiet
proof and drain.

The corrected baseline also sharpens the next constraint. Replacing Redb with
the in-memory semantic oracle removes about 1.28 seconds, but the resulting
91,867 events/s remains 38.8% below the epic gate. Storage is still the largest
single measured owner, yet storage alone cannot finish #612; parse,
verification, serial application, and projection delivery still have a combined
ceiling below the target.

## Duplicate replay

One fresh two-pass Redb run separated the first 100,000 inserts from the next
100,000 duplicate frames. Replay completed in 1,839.2 ms, or 54,371 duplicate
frames/s. The old aggregate divided 200,000 frames by the complete initial plus
replay interval and could not express duplicate-only performance. The corrected
result is now comparable to the epic's 500,000 events/s replay gate and shows
that duplicate handling is 9.2x short, despite the verified-event cache already
eliminating repeated signature verification.

Historical result files remain unchanged. Their schema names and raw durations
preserve what they actually measured; comparisons against v10 must choose the
clock explicitly.
