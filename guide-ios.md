# Build an iOS app with NMP

This is the shortest realistic path to an iOS NIP-29 client: build one NMP
package with the capabilities your app uses, construct one engine, create an
account, browse groups on relays your app knows, join one, and send messages.

## 1. Build your NMP package

Check a feature manifest into your app repository as `nmp.toml`:

```toml
schema = 1
features = ["nip29", "nip65", "nipc7"]
```

- `nip29` supplies group discovery, records, membership operations, and
  group-scoped reads/writes.
- `nip65` supplies automatic outbox routing. Your app still chooses the
  indexer relays used to discover relay lists.
- `nipc7` supplies the kind:9 chat and reply composers.

From an NMP checkout, with Python 3.11+, Rust, and Xcode installed:

```bash
scripts/nmp-native prepare \
  --manifest /path/to/MyApp/nmp.toml \
  --platform apple \
  --output /path/to/MyApp/Generated/NMP
```

The command builds or reuses one content-addressed native library, generates
UniFFI from that exact library, and materializes only the selected Swift
wrappers. In Xcode, add `Generated/NMP/apple` as a local package and add its
`NMP` product to your app target. App code imports only `NMP`, never `NMPFFI`.

Run the same command in clean CI. Ordinary Xcode builds consume the generated
local package and do not run Cargo.

## 2. Construct one engine

Create the engine once for the app's lifetime. The event cache and durable
publish queue live at `storePath`; the signing key checkpoint is a separate
Keychain item.

```swift
import Foundation
import NMP

func makeNMP(indexerRelays: [String]) throws -> NMPEngine {
    let directory = try FileManager.default.url(
        for: .applicationSupportDirectory,
        in: .userDomainMask,
        appropriateFor: nil,
        create: true
    )

    return try NMPEngine(
        config: NMPConfig(
            storePath: directory.appendingPathComponent("nmp.redb").path,
            nip65: NIP65Config(indexerRelays: indexerRelays)
        ),
        localAccountStore: NMPKeychainAccountStore(
            service: "com.example.myapp.nmp-account"
        )
    )
}
```

NMP supplies no hidden relay. Pass at least one app-owned indexer when NIP-65
is enabled. On a later launch, the Keychain checkpoint is restored and made
active during `NMPEngine` initialization.

## 3. Create and activate an account

```swift
let registration = try await nmp.generateAccount()
try nmp.setActiveAccount(registration.publicKey)

let myPubkeyHex = registration.publicKey
```

`generateAccount()` registers a new local signer and checkpoints it in the
configured Keychain store. It deliberately does not choose the active account;
that second line is the app's explicit account-selection decision. Importing
an existing account is the same flow with
`try await nmp.addAccount(secretKey: pastedNsec)`.

## 4. Find NIP-29 groups

NIP-29 groups are relay-scoped. Start with group relay URLs learned by your
app—for example from a link, app configuration, or a remembered-groups list—
then observe the directory those relays advertise:

```swift
let groupRelays = try NMPRelayScope.on([
    "wss://groups.example.com"
])

let directory = try groupRelays.observeRecords(
    engine: nmp,
    matching: .all,
    records: [.metadata],
    limit: 100
)

for try await groups in directory {
    let rows = groups.map { group in
        (id: group.id, name: group.metadata?.name ?? group.id)
    }
    // Replace your app's directory state with `rows`.
}
```

Each delivery is the complete current snapshot, not a patch. `.all` means
"every group for which these relays advertise the requested record"; it is
not a claim that every group hosted there is enumerable.

## 5. Open and join a group

```swift
let group = groupRelays.group(selectedGroupID) // contacts nothing yet

let joinReceipt = try group.joinRequest(
    engine: nmp,
    authorPubkeyHex: myPubkeyHex
)
let joinDelivery = try await joinReceipt.result()

print(joinDelivery.outcome)
for relay in joinDelivery.relays {
    print(relay.relay, relay.state)
}
```

The returned receipt is NMP's ordinary durable write receipt. Its result says
what each destination relay did with the request; it does not claim that a
closed group admitted the user. Observe the group's relay-signed records for
the state the relay exposes:

```swift
let room = try group.observeRecords(
    engine: nmp,
    records: [.metadata, .admins, .members]
)

for try await snapshot in room {
    let title = snapshot.metadata?.name ?? selectedGroupID
    let listedMembers = snapshot.members
    let availability = snapshot.availability
    // Render your app's room state.
}
```

A NIP-29 member list is optional and may be partial. Presence is evidence;
absence is not proof that somebody is not a member.

## 6. Observe and send chat messages

The group contributes the `h` tag and exact relay scope. NIP-C7 owns the
kind:9 message schema.

```swift
let messages = try nmp.observe(
    group.read(NMPFilter(kinds: [9], limit: 100))
)

for try await batch in messages {
    let newestFirst = batch.rows.sorted { $0.createdAt > $1.createdAt }
    // Replace your app's message state with `newestFirst`.
}
```

```swift
let payload = try chat().withContent([
    .text("Hello from my NMP app")
])

let messageReceipt = try group.publish(
    engine: nmp,
    authorPubkeyHex: myPubkeyHex,
    payload: payload
)
let messageDelivery = try await messageReceipt.result()
```

Your app owns screens, navigation, ordering, moderation, and how relay
evidence is explained. NMP owns the live subscriptions, canonical cache,
signing, group context, routing, retry, and durable per-relay receipts.

## Current qualification

The generated package contains iOS device and simulator slices. The public
Swift wrapper behavior is currently qualified by macOS-host tests; iOS runtime
and physical-device qualification remain separate evidence.
