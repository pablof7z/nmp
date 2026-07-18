# Issue #613 — verified-event clone fan-out

## Verdict

Close the production change negative. The ownership prototype removed the
four fixed resolver `Event` clones on every ordinary Redb insertion, but the
clean production effect was not material:

- Redb direct-median throughput improved **0.7%**; the paired median improved
  **2.4%**, with only 6 of 10 pairs winning.
- The controlled MemoryStore production path improved **0.7%** by direct
  median and **0.3%** by paired median, with 7 of 10 pairs winning.
- Absolute Redb peak RSS improved **2.3%** and process writes were unchanged.

The candidate was reverted rather than merging roughly three hundred lines of
ownership machinery for a result inside run-to-run noise. The opt-in clone and
allocator counters remain useful production-probe instrumentation.

## Exact clone and allocation result

The instrumentation-only baseline (`132bb99`) measured the existing ordinary
100k Redb path at:

- transport fallback clones: 0;
- resolver clones: 400,000 (exactly 4 per input);
- store clones: 0;
- committed projection materializations: 100,000 (exactly 1 per delivered
  row).

The ownership candidate (`868d8f6`) reduced resolver clones to **0** while the
other counts stayed unchanged. In the instrumented A/B it also reduced:

- allocation operations from 11,348,332 to 8,609,950 (**24.1%**);
- cumulative requested allocation bytes from 3,744,048,466 to 3,317,574,106
  (**11.4%**);
- resolver prepare plus classification time from 234.0 ms to 56.1 ms
  (**76.0%**).

That run improved full throughput by 2.8%, but it is attribution evidence, not
the decision result: the global allocator and clone counters use relaxed
atomics across the server, transport, verifier, and engine threads. The clean
matrices below were built without that instrumentation.

## Clean Redb production matrix

Ten fresh-process alternating pairs, 100k representative events, Immediate
durability:

| Metric | Baseline median | Candidate median | Change |
|---|---:|---:|---:|
| Throughput | 28,899 events/s | 29,106 events/s | +0.7% |
| Full ingest wall | 3,460.3 ms | 3,448.7 ms | -0.3% |
| Absolute peak RSS | 123.1 MB | 120.3 MB | -2.3% |
| Process writes | 246.3 MB | 247.0 MB | +0.3% |

The paired throughput ratios ranged from -14.0% to +23.1%; their median was
+2.4%. This spread is much larger than the direct-median result, so the matrix
does not support a meaningful throughput claim.

## Controlled MemoryStore production matrix

Ten more alternating production-probe pairs removed disk/commit variance
while retaining parse, verify, transport, resolver, projection, and delivery:

| Metric | Baseline median | Candidate median | Change |
|---|---:|---:|---:|
| Throughput | 45,463 events/s | 45,772 events/s | +0.7% |
| Full ingest wall | 2,199.6 ms | 2,184.7 ms | -0.7% |
| Absolute peak RSS | 643.2 MB | 648.4 MB | +0.8% |

The paired throughput median was +0.3%. This cleaner ceiling confirms that
removing the resolver clones does not own a material share of production wall
time.

## Architectural decision

The prototype used a resolver-private committed-batch owner and a hidden
borrowed `EventStore` batch door. Redb encoded borrowed events, then the
resolver moved the original values into committed row changes; `react` and
affected-handle matching borrowed those rows. All focused store/resolver/engine
falsifiers passed, including provenance growth, supersession, same-batch
insert-delete collapse, expiry, and multi-owner pending adoption.

That shape is sound, but it is not a prerequisite for a selected next step:
the direct packed parser proposal in #615 closed negative, while the current
packed-postings event plane already indexes borrowed `Event` fields inside its
governed transaction. Keeping the extra ownership surface would therefore be
speculative machinery, contrary to #613's negative-result rule.

