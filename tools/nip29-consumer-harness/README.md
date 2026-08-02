# NIP-29 consumer relay harness

This is the relay fixture for issue #1200 and the consumer capstone in #1140.
It runs two isolated, unrestricted Croissant group relays and seeds signed
protocol inputs. Croissant and the seeding client are infrastructure and
independent witnesses; only the Rust and Swift apps using NMP count as product
evidence.

The harness pins Croissant commit
`9c4c93e84852bd9aa6824060b74c56ab2ce812c2`. It generates disposable signing
keys inside the run directory, never prints them, and writes only public keys
and signed events to the public manifest and seed artifacts.

## Requirements

- Docker
- Git
- `nak` 0.19.5 or newer (fixture seeding and independent relay inspection only)
- `curl`, `jq`, `sha256sum`, and `timeout`

## One-command start and seed

```sh
tools/nip29-consumer-harness/harness.sh start /tmp/nmp-nip29-run
```

The command starts relays at `ws://127.0.0.1:19888` and
`ws://127.0.0.1:19889`, seeds the complete topology, and prints the public
manifest path. Pass `--port-a` and `--port-b` before the run directory to use
different ports.

The seed includes:

- `solo-a`, hosted only on relay A with its own kind 9 content row;
- `bitcoin`, hosted independently on both relays with conflicting relay-signed
  metadata and divergent member evidence;
- `one-sided`, hosted only on relay A so a scope containing A and B can produce
  one acceptance and one typed relay rejection;
- one viewer contact-list replacement input and positive member-list inclusion
  evidence for a followed subject;
- shared signed kind 9 and kind 30023 events published to both relays, plus
  relay-specific events and dense timestamp ties; and
- public relay witness logs and post-seed JSONL snapshots.

The groups are open and unrestricted. The harness never configures a supported
kind allow-list.

## Fault and mutation controls

```sh
tools/nip29-consumer-harness/harness.sh metadata-conflict /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh chat-append /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh follow-remove /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh follow-add /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh relay-down a /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh relay-up a /tmp/nmp-nip29-run
tools/nip29-consumer-harness/harness.sh status /tmp/nmp-nip29-run
```

These controls stage causes at the relays. They do not insert routes, resolved
group ids, branch evidence, or union results into NMP.

## Teardown

```sh
tools/nip29-consumer-harness/harness.sh stop /tmp/nmp-nip29-run
```

`stop` captures each relay's stdout/wire log, removes both containers, verifies
that both public ports are released, and retains the run directory for the
evidence report. Delete that explicit directory only after its evidence is no
longer needed.
