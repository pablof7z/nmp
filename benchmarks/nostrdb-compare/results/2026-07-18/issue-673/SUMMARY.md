# Issue #673 — parse, event-ID, and Schnorr ceiling

## Decision

Close the standalone parse/validation direction negative. Keep the
benchmark-only attribution and favorable ceilings; do not ship a parser or
cryptography replacement from this evidence.

On the storage-free MemoryStore path, trusting every relay-provided event ID
and signature did not improve the complete pipeline. The median was 1.7%
slower than the fully validating baseline. Preloading every owned event so the
wire path performed no EVENT JSON parse was 4.6% slower and raised absolute
peak RSS by 15.7%. Neither candidate approached the 10% production gate.

These are deliberately unsafe ceilings, compiled only with
`bench-instrumentation`. Ordinary builds retain exact NIP-01 ID construction
and Schnorr verification.

## Complete representative MemoryStore pipeline

Every run crossed the websocket, transport, ordered engine bridge,
MemoryStore semantic oracle, bounded history projection, and observer
delivery. Every report observed exactly 100,000 relay EVENT frames and ended
with exactly 200 visible rows.

| Mode | Median throughput | Change | Median wall | Change |
| --- | ---: | ---: | ---: | ---: |
| Exact baseline | 89,432 events/s | — | 1,118.2 ms | — |
| Trust event ID | 88,271 events/s | 1.3% lower | 1,132.9 ms | 1.3% higher |
| Trust signature | 85,010 events/s | 4.9% lower | 1,176.3 ms | 5.2% higher |
| Trust ID and signature | 87,885 events/s | 1.7% lower | 1,137.9 ms | 1.8% higher |
| Preparsed owned events | 85,354 events/s | 4.6% lower | 1,171.6 ms | 4.8% higher |

Values are independent medians of 3 fresh processes. Mode order was rotated
for the validation matrix. The preparsed ceiling retained the exact generated
events before ingest and moved them into the ordinary typed frame path in wire
order. It recorded 100,000 cache hits and only the non-EVENT control parse.

## Attribution

The exact baseline medians were:

- Relay JSON parse and owned construction: 390.1 ms of socket-worker CPU.
- Canonical event-ID serialization and hash: 150.7 ms on the translator.
- Verifier batch wall: 876.5 ms.
- Summed Schnorr worker CPU: 4,821.5 ms across 8 workers.

Those values overlap. Parsing runs on the socket worker while Schnorr work,
engine reduction, storage, and projection run on other threads. Removing an
overlapped CPU bucket therefore does not imply the same reduction in complete
wall time. The complete favorable ceilings are the decision metric.

Even an overgenerous arithmetic bound that subtracts the entire 389.9 ms parse
CPU from the already-unsafe ID-plus-signature wall yields about 133,700
events/s. A standalone direct parser still cannot meet #612's 150,000
events/s gate.

## Consequence for #612

The load-bearing storage-free path is now the serial engine-applied work, not
the parser, event-ID hash, Schnorr implementation, or verifier channels in
isolation. In the median baseline report, resolver work accumulated 575.9 ms
and committed history projection accumulated 235.3 ms; the bridge waited for
applied engine batches throughout ingest.

#667 proved history materialization can fall 85% but moved complete throughput
only 6%. #671 proved verifier messages can fall 98% with no throughput gain.
The next exact experiment should combine already-proven favorable stage
ceilings before rejecting them independently. If the combined ceiling clears
150,000 events/s, production work must be selected as a coherent pipeline
change; if it does not, the epic target is not reachable through these stages.
