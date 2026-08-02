# NMP NIP-29 Rust consumer

This is the direct-Rust application falsifier for #1201 and parent #1140. Its
`Cargo.toml` depends on `nmp` and nothing else. It exercises the supported WIP
NIP-29 facade against two real, unrestricted local relays.

Croissant is only the relay process. `nak` is only the fixture signer/seeder
and an independent relay witness. Neither participates in any assertion made
by this program. The program obtains rows, per-relay provenance, acquisition
evidence, exact wire filters, diagnostics, signatures, routes, and publication
outcomes exclusively through NMP.

## Full run

From the repository root, choose two paths that do not yet exist:

```sh
tools/nip29-consumer/run-capstone.sh \
  /tmp/nmp-nip29-rust-run \
  /tmp/nmp-nip29-rust-evidence
```

The runner stages causes only: it starts/stops the two relays, publishes
controlled fixture mutations, and launches the consumer's five modes. It never
supplies NMP with routes, resolved ids, rows, or evidence. The modes prove:

- `online`: follows-derived positive group discovery, single-host and
  multi-host reads, separately app-selected kinds 9 and 30023, duplicate
  collapse with two-relay provenance, conflicting relay metadata with an
  app-selected display winner, slow consumption, a 3-to-8 bounded window,
  exact filter/coverage diagnostics, two-host publication, and a mixed
  one-ack/one-rejection publication;
- `live-adversarial`: two identical observations share one exact wire filter
  per relay; cancelling one preserves the surviving demand, which receives a
  new two-source chat while already-live metadata and follows-derived queries
  observe relay-specific replacement, removal, and re-addition; cancelling the
  survivor then removes both group filters;
- `provenance-growth`: with relay B initially down, an existing row grows
  from relay-A provenance to both relays after NMP reconnects, delivered as
  `RowDelta::SourcesGrew`; and
- `restart`: a fresh NMP engine over the same persistent store emits cached
  rows and retained per-source watermarks while both relays are offline, then
  resumes live acquisition after they return; and
- `restart-conflict`: another fresh engine retains both relay-specific live
  metadata replacements from cache while relay A is requesting and relay B is
  offline, then refreshes both branches after relay B reconnects.

The evidence directory retains stdout logs, the two NMP stores, lifecycle
logs, and a compact `proof-lines.txt`. The harness run directory retains the
signed relay inputs and wire witnesses. Secrets remain only in that private
run directory and are never copied into consumer evidence.
