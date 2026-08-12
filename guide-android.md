# Build an Android app with NMP

This is the shortest realistic path to an Android NIP-29 client: build one AAR
with the capabilities your app uses, construct one engine, create an account,
browse groups on relays your app knows, join one, and send messages.

## 1. Build your NMP AAR

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

From an NMP checkout, with Python 3.11+, Rust, JDK 17, Android SDK 35, and NDK
27.2.12479018 installed:

```bash
scripts/nmp-native prepare \
  --manifest /path/to/MyApp/nmp.toml \
  --platform android \
  --output /path/to/MyApp/Generated/NMP
```

The command builds or reuses one content-addressed native library for the
governed Android ABIs, generates Android-mode UniFFI from that exact library,
materializes only the selected Kotlin wrappers, and publishes the result to a
local Maven repository.

Point Gradle at that repository:

```kotlin
// settings.gradle.kts
dependencyResolutionManagement {
    repositories {
        maven { url = uri("$rootDir/Generated/NMP/android/repository") }
        google()
        mavenCentral()
    }
}
```

```kotlin
// app/build.gradle.kts
dependencies {
    implementation("com.nmp:nmp-android:0.0.0")
}
```

The `0.0.0` coordinate is intentionally local; the generated provenance file
identifies the actual source, feature set, tools, ABIs, and artifact bytes.
App code imports only `com.nmp.sdk`, never `uniffi.nmp_ffi`.

Run the same prepare command in clean CI. Ordinary Gradle builds consume the
generated repository and do not run Cargo.

## 2. Construct one engine

Create the engine once in your application scope. `storePath` holds the event
cache and durable publish queue; it does not store the account secret.

```kotlin
import android.content.Context
import com.nmp.sdk.NIP65Config
import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPEngine
import java.io.File

fun makeNMP(context: Context, indexerRelays: List<String>): NMPEngine =
    NMPEngine(
        NMPConfig(
            storePath = File(context.filesDir, "nmp.redb").absolutePath,
            nip65 = NIP65Config(indexerRelays),
        ),
    )
```

NMP supplies no hidden relay. Pass at least one app-owned indexer when NIP-65
is enabled.

The Android AAR does not currently ship a standard Android Keystore account
checkpoint. The engine above is usable, but a generated/imported local signer
must be registered again after process restart. A production app can pass its
own `NMPLocalAccountCheckpoint` implementation as `localAccountStore`; do not
use `NMPInsecureFileAccountStore` for production keys.

## 3. Create and activate an account

```kotlin
val registration = nmp.generateAccount()
nmp.setActiveAccount(registration.publicKey)

val myPubkeyHex = registration.publicKey
```

`generateAccount()` registers a new local signer. It deliberately does not
choose the active account; that second line is the app's explicit
account-selection decision. Importing an existing account is the same flow
with `nmp.addAccount(pastedNsec)`.

## 4. Find NIP-29 groups

NIP-29 groups are relay-scoped. Start with group relay URLs learned by your
app—for example from a link, app configuration, or a remembered-groups list—
then collect the directory those relays advertise:

```kotlin
import com.nmp.sdk.NMPGroupPredicate
import com.nmp.sdk.NMPGroupRecord
import com.nmp.sdk.NMPRelayScope
import kotlinx.coroutines.flow.collectLatest

val groupRelays = NMPRelayScope.on(
    listOf("wss://groups.example.com"),
)

groupRelays.observeRecords(
    engine = nmp,
    predicate = NMPGroupPredicate.all(),
    records = listOf(NMPGroupRecord.Metadata),
    limit = 100u,
).collectLatest { groups ->
    val rows = groups.map { group ->
        group.id to (group.metadata?.name ?: group.id)
    }
    // Replace your app's directory state with `rows`.
}
```

Each emission is the complete current snapshot, not a patch. `all()` means
"every group for which these relays advertise the requested record"; it is
not a claim that every group hosted there is enumerable.

## 5. Open and join a group

Run the suspending receipt call from a coroutine:

```kotlin
val group = groupRelays.group(selectedGroupId) // contacts nothing yet

val joinReceipt = group.joinRequest(
    engine = nmp,
    authorPubkeyHex = myPubkeyHex,
)
val joinDelivery = joinReceipt.result()

println(joinDelivery.outcome)
joinDelivery.relays.forEach { println("${it.relay}: ${it.state}") }
```

The returned receipt is NMP's ordinary durable write receipt. Its result says
what each destination relay did with the request; it does not claim that a
closed group admitted the user. Collect the group's relay-signed records for
the state the relay exposes:

```kotlin
group.observeRecords(
    engine = nmp,
    records = listOf(
        NMPGroupRecord.Metadata,
        NMPGroupRecord.Admins,
        NMPGroupRecord.Members,
    ),
).collectLatest { snapshot ->
    val title = snapshot.metadata?.name ?: selectedGroupId
    val listedMembers = snapshot.members
    val availability = snapshot.availability
    // Render your app's room state.
}
```

A NIP-29 member list is optional and may be partial. Presence is evidence;
absence is not proof that somebody is not a member.

## 6. Observe and send chat messages

The group contributes the `h` tag and exact relay scope. NIP-C7 owns the
kind:9 message schema.

```kotlin
import com.nmp.sdk.NMPFilter

nmp.observe(
    group.read(NMPFilter(kinds = listOf(9u.toUShort()), limit = 100u)),
).collectLatest { batch ->
    val newestFirst = batch.rows.sortedByDescending { it.createdAt }
    // Replace your app's message state with `newestFirst`.
}
```

```kotlin
import com.nmp.sdk.ContentPart
import com.nmp.sdk.chat
import com.nmp.sdk.withContent

val payload = chat().withContent(
    listOf(ContentPart.Text("Hello from my NMP app")),
)
val messageReceipt = group.publish(
    engine = nmp,
    authorPubkeyHex = myPubkeyHex,
    payload = payload,
)
val messageDelivery = messageReceipt.result()
```

Your app owns screens, navigation, ordering, moderation, and how relay
evidence is explained. NMP owns the flows, canonical cache, signing, group
context, routing, retry, and durable per-relay receipts. Call `nmp.close()`
when the application-scoped engine is genuinely finished.

## Current qualification

The generated API-26 AAR is packaging- and consumer-build-qualified for
`arm64-v8a` and `x86_64`. Emulator and physical-device runtime qualification
remain separate evidence.
