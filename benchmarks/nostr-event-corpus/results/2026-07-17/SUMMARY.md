# Issue #620 representative event corpus

## Capture

The accepted sample contains 22,408 exact public relay `EVENT` frames from
3,024 two-second windows (504 per relay) spanning 2026-07-10 through
2026-07-16. All 22,408 observations had valid event ids and signatures; there
were no malformed frames, invalid events, or conflicting uses of one event id.
There are 13,619 unique events and 8,789 duplicate observations (39.22%).

The six accepted relays are Damus, nos.lol, nostr.mom, offchain.pub,
relay.primal.net, and relay.nostr.wirednet.jp. No accepted window hit the
requested 5,000-event ceiling. `relay.nostr.net` was rejected because 3 of 504
pilot windows hit its hidden 100-event ceiling. `nostr.bitcoiner.social` and
`relay.snort.social` returned no historical events. The raw frames are not
redistributed because they include public users' content and identifiers; the
acquisition script plus aggregate BLAKE3
`d50e02c7d30928d96930ea7c0d51e34ef9e3e085b0461663d6d0154fd2c92878`
is the reproducibility boundary.

## Distribution

Unique-event raw frame sizes are 525 B p50, 669 B p75, 1,690 B p90,
2,595 B p95, 10,543 B p99, 183,783 B p99.9, and 444,613 B max. Of unique
events, 44.48% are at most 512 B, 86.17% at most 1 KiB, 91.53% at most
2 KiB, 96.84% at most 4 KiB, and 99.25% at most 16 KiB. No complete signed
Nostr event frame was at most 256 B.

Decoded content is 5 B p50, 203 B p75, 944 B p90, 2,140 B p95, and
10,018 B p99. Tag count is 1 p50, 2 p75, 3 p90, 5 p95, and 9 p99;
encoded tag bytes are 74 B p50, 141 B p75, 158 B p90, 258 B p95, and
845 B p99.

The dominant unique kinds are kind 5 (5,575; 40.94%), kind 1 (4,991;
36.65%), kind 1059 (882; 6.48%), kind 7 (681; 5.00%), and kind 4 (275;
2.02%). The committed 10,000-shape corpus is a deterministic proportional
sample of the 13,188 unique frames at most 4 KiB, stratified by kind, frame
size bucket, and tag-count bucket. It retains only byte costs, public protocol
tag-name classes, and coarse value classes. The committed recursive string
inventory contains only 51 schema/class labels, public tag names, single-letter
tag names, explanatory metadata, and the source hash; it is an independently
inspectable privacy falsifier.

## Production-path result

Three same-host repetitions alternated the prior uniform 100k x 128-byte
workload with the representative shape workload. Both use the production
transport, signature verification, resolver, governed Redb store, live-query
delivery, shutdown, and exact reopen verification.

| Median | Uniform | Representative | Change |
| --- | ---: | ---: | ---: |
| Active ingest | 67,829 events/s | 25,351 events/s | -62.6% (2.68x slower) |
| Ingest including 1 s quiet proof | 38,834 events/s | 19,819 events/s | -49.0% |
| Store transactions | 1,123 ms | 3,227 ms | 2.87x |
| Commit | 487 ms | 1,328 ms | 2.72x |
| Index insertion | 226 ms | 671 ms | 2.97x |
| Flush | 0.426 ms | 498 ms | 1,169x |
| Parse | 227 ms | 512 ms | 2.26x |
| Verify | 924 ms | 1,125 ms | 1.22x |
| p95 apply latency | 538 ms | 1,499 ms | 2.79x |
| First row | 11.895 ms | 13.369 ms | +12.4% |
| Peak RSS | 144.75 MB | 177.32 MB | +22.5% |
| Redb file | 134.75 MB | 269.49 MB | 2.00x |

The representative workload therefore replaces, rather than merely
supplements, the uniform tiny-event workload for #612. Its 100k median is
6.0x below the 150k/s completion gate. Eliminating all measured resolver
prepare/classify work or all JSON parse work would not close that gap; the
dominant measured cost is the governed write path, especially index insertion,
commit, and flush behavior. This is negative evidence against treating #613
clone cleanup or #615 packed parsing as the next multiplier-sized change.

## Scale and replay

The one-million representative run persisted and reopened exactly 1,000,000
events. It processed 19,479 frames/s including the quiet proof (19,906/s to
the last visible-row update), peaked at 191.38 MB RSS with 180.06 MB growth,
and produced a 2.466 GB Redb file. Store transactions consumed 44.16 of
50.24 active seconds; index insertion consumed 12.33 s and flush 9.36 s.

The two-pass representative run processed 200,000 frames and reopened exactly
100,000 canonical events at 49,664 aggregate frames/s including quiet proof,
far below the 500k/s duplicate gate. The aggregate includes the first write,
so it is deliberately not labeled a duplicate-only rate. Instrumentation did
show only 100,000 verification candidates, proving the second pass hit the
verified-event cache.

## Decision

#620 quantifies the real distribution and defines the parent epic's primary
100k and one-million corpora. The next optimization should first isolate and
then remove representative-event governed index/flush amplification. Redb is
the current implementation, not a fixed constraint: a follow-up may compare a
different physical layout or storage engine, provided it preserves equivalent
crash atomicity/exact reopen and quantifies native plus future-WASM portability
costs. A new implementation issue should be filed only after that consequence
and its falsifier are stated; this result does not justify lowering #612's
gates.
