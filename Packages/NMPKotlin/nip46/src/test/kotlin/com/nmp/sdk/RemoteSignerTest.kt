package com.nmp.sdk

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.URLEncoder
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.Base64
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue
import kotlin.time.Duration.Companion.milliseconds
import kotlin.time.Duration.Companion.seconds
import uniffi.nmp_nip46_ffi.FfiNip46Compatibility
import uniffi.nmp_nip46_ffi.FfiNip46ConnectionEvent
import uniffi.nmp_nip46_ffi.FfiNip46Failure
import uniffi.nmp_nip46_ffi.FfiNip46PreparedConnection
import uniffi.nmp_nip46_ffi.Nip46ConnectionObserver
import uniffi.nmp_nip46_ffi.NMP_NIP46_PACKAGED_COMPONENT_IDENTITY
import uniffi.nmp_nip46_ffi.nmpNip46ComponentIdentity
import uniffi.nmp_nip46_ffi.prepareNip46Bunker
import uniffi.nmp_nip46_ffi.verifyNip46Component

private class KotlinWebSocketBlackhole(
    holdHandshake: Boolean = false,
) : AutoCloseable {
    private val server = ServerSocket(0, 16, InetAddress.getLoopbackAddress())
    private val clients = CopyOnWriteArrayList<Socket>()
    private val accepted = CountDownLatch(1)
    private val handshakeRelease = CountDownLatch(if (holdHandshake) 1 else 0)
    val relay = "ws://127.0.0.1:${server.localPort}"
    private val worker = Thread {
        while (!server.isClosed) {
            val socket = try {
                server.accept()
            } catch (_: Exception) {
                return@Thread
            }
            if (completeHandshake(socket)) {
                clients += socket
                accepted.countDown()
            } else {
                socket.close()
            }
        }
    }.apply {
        isDaemon = true
        name = "nmp-kotlin-websocket-blackhole"
        start()
    }

    private fun completeHandshake(socket: Socket): Boolean {
        val reader = socket.getInputStream().bufferedReader()
        var key: String? = null
        while (true) {
            val line = reader.readLine() ?: return false
            if (line.isEmpty()) break
            if (line.startsWith("Sec-WebSocket-Key:", ignoreCase = true)) {
                key = line.substringAfter(':').trim()
            }
        }
        val exactKey = key ?: return false
        if (!handshakeRelease.await(2, TimeUnit.SECONDS)) return false
        val digest = MessageDigest.getInstance("SHA-1").digest(
            (exactKey + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
                .toByteArray(StandardCharsets.UTF_8),
        )
        val accept = Base64.getEncoder().encodeToString(digest)
        socket.getOutputStream().bufferedWriter().apply {
            write("HTTP/1.1 101 Switching Protocols\r\n")
            write("Upgrade: websocket\r\n")
            write("Connection: Upgrade\r\n")
            write("Sec-WebSocket-Accept: $accept\r\n\r\n")
            flush()
        }
        return true
    }

    fun releaseHandshake() = handshakeRelease.countDown()

    fun awaitHandshake(): Boolean = accepted.await(2, TimeUnit.SECONDS)

    override fun close() {
        server.close()
        clients.forEach { it.close() }
        worker.join(2_000)
    }
}

private class NativeCloseRecord {
    private val action = AtomicReference<(() -> Boolean?)?>(null)
    private val results = mutableListOf<Boolean?>()
    private val recorded = CountDownLatch(1)

    fun setAction(value: () -> Boolean?) {
        action.set(value)
    }

    @Synchronized
    fun record() {
        results += action.get()?.invoke()
        recorded.countDown()
    }

    @Synchronized
    fun snapshot(): List<Boolean?> = results.toList()

    fun awaitSnapshot(): List<Boolean?> {
        assertTrue(recorded.await(2, TimeUnit.SECONDS))
        return snapshot()
    }
}

private class ForwardingObserver(
    private val projected: NMPNip46Observer,
    private val closes: NativeCloseRecord,
) : Nip46ConnectionObserver {
    override fun onEvent(event: FfiNip46ConnectionEvent) = projected.onEvent(event)
    override fun onReady(userPublicKey: String) = projected.onReady(userPublicKey)
    override fun onFailed(failure: FfiNip46Failure) = projected.onFailed(failure)
    override fun onClosed() {
        closes.record()
        projected.onClosed()
    }
}

@OptIn(NMPProviderComponentApi::class)
class RemoteSignerTest {
    private val remoteSignerPublicKey =
        "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private fun compatibility(): FfiNip46Compatibility = verifyNip46Component(
        NMP_NIP46_PACKAGED_COMPONENT_IDENTITY,
        nmpNip46ComponentIdentity(),
        nmpProviderComponentInterfaceIdentity(),
        nmpProviderCoreComponentIdentity(),
    )

    private fun prepare(
        blackhole: KotlinWebSocketBlackhole,
        compatibility: FfiNip46Compatibility,
        observer: Nip46ConnectionObserver,
        timeoutMillis: ULong = 30_000u,
    ): FfiNip46PreparedConnection {
        val relay = URLEncoder.encode(blackhole.relay, StandardCharsets.UTF_8)
        val uri = "bunker://$remoteSignerPublicKey?relay=$relay&secret=nmp-kotlin-native"
        return prepareNip46Bunker(compatibility, uri, timeoutMillis, observer)
    }

    @Test
    fun catalogContainsOnlyNip46DetectionLaunchAndPackageFacts() {
        val primal = NMPNip46SignerDiscovery.known.single { it.id == "primal" }
        assertEquals("primalconnect://probe", primal.iosDetectionUri)
        assertEquals("primalconnect", primal.nip46LaunchScheme)
        assertEquals("primal://signer", primal.androidDetectionUri)
        assertEquals("net.primal.android", primal.androidPackageId)
        assertEquals(listOf("primal"), NMPNip46SignerDiscovery.known.map { it.id })
    }

    @Test
    fun androidDiscoveryIsPackageFilteredWhenSchemesAreShared() {
        assertEquals(
            emptyList(),
            NMPNip46SignerDiscovery.installedAndroid(setOf("com.greenart7c3.nostrsigner")).map { it.id },
        )
        assertEquals(
            listOf("primal"),
            NMPNip46SignerDiscovery.installedAndroid(setOf("net.primal.android")).map { it.id },
        )
        assertEquals(
            listOf("primal"),
            NMPNip46SignerDiscovery.installedAndroid(
                setOf("net.primal.android", "com.greenart7c3.nostrsigner"),
            ).map { it.id },
        )
    }

    @Test
    fun primalInvitationChangesOnlyTheLaunchScheme() {
        NMPEngine(NMPConfig()).use { engine ->
            val invitation = engine.nip46Invitation(listOf("wss://relay.example"))
            val generic = invitation.uri()
            val primal = NMPNip46SignerDiscovery.known.single { it.id == "primal" }
            val appSpecific = invitation.uri(primal)
            assertTrue(generic.startsWith("nostrconnect://"))
            assertTrue(appSpecific.startsWith("primalconnect://"))
            assertEquals(
                generic.removePrefix("nostrconnect"),
                appSpecific.removePrefix("primalconnect"),
            )
            assertEquals(
                NMPAndroidSignerHandoff(
                    uri = appSpecific,
                    packageName = "net.primal.android",
                ),
                invitation.androidHandoff(primal),
            )
            val forged = primal.copy(
                androidPackageId = "attacker.example",
            )
            assertEquals(
                "net.primal.android",
                invitation.androidHandoff(forged).packageName,
                "handoff must resolve package and protocol from the Rust catalog by id",
            )
        }
    }

    @Test
    fun mismatchedCoreIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        val error = assertFailsWith<NMPError.NativeComponentMismatch> {
            withVerifiedNip46Component(
                NMP_NIP46_PACKAGED_COMPONENT_IDENTITY,
                nmpNip46ComponentIdentity(),
                nmpProviderComponentInterfaceIdentity(),
                "deliberately-mismatched-core",
            ) {
                adapterPreparationRan = true
            }
        }
        assertEquals("nmp-nip46", error.component)
        assertTrue(error.expectedIdentity.startsWith("nmp-core-component-v2-"))
        assertEquals("deliberately-mismatched-core", error.actualIdentity)
        assertFalse(adapterPreparationRan)
    }

    @Test
    fun mismatchedPackagedInterfaceIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        val error = assertFailsWith<NMPError.NativeComponentMismatch> {
            withVerifiedNip46Component(
                NMP_NIP46_PACKAGED_COMPONENT_IDENTITY,
                nmpNip46ComponentIdentity(),
                "deliberately-mismatched-interface",
                nmpProviderCoreComponentIdentity(),
            ) {
                adapterPreparationRan = true
            }
        }
        assertEquals("nmp-nip46", error.component)
        assertTrue(error.expectedIdentity.startsWith("nmp-component-interface-v2-"))
        assertEquals("deliberately-mismatched-interface", error.actualIdentity)
        assertFalse(adapterPreparationRan)
    }

    @Test
    fun mismatchedPackagedProviderBindingIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        val error = assertFailsWith<NMPError.NativeComponentMismatch> {
            withVerifiedNip46Component(
                "deliberately-mismatched-binding",
                nmpNip46ComponentIdentity(),
                nmpProviderComponentInterfaceIdentity(),
                nmpProviderCoreComponentIdentity(),
            ) {
                adapterPreparationRan = true
            }
        }
        assertEquals("nmp-nip46", error.component)
        assertTrue(error.expectedIdentity.startsWith("nmp-nip46-component-v2-"))
        assertEquals("deliberately-mismatched-binding", error.actualIdentity)
        assertFalse(adapterPreparationRan)
    }

    @Test
    fun mismatchedLoadedProviderNativeIsTypedBeforeAdapterPreparationRuns() {
        var adapterPreparationRan = false

        val error = assertFailsWith<NMPError.NativeComponentMismatch> {
            withVerifiedNip46Component(
                NMP_NIP46_PACKAGED_COMPONENT_IDENTITY,
                "deliberately-mismatched-native",
                nmpProviderComponentInterfaceIdentity(),
                nmpProviderCoreComponentIdentity(),
            ) {
                adapterPreparationRan = true
            }
        }
        assertEquals("nmp-nip46", error.component)
        assertTrue(error.expectedIdentity.startsWith("nmp-nip46-component-v2-"))
        assertEquals("deliberately-mismatched-native", error.actualIdentity)
        assertFalse(adapterPreparationRan)
    }

    @Test
    fun connectionStatesMulticastReadyAndClosedToEveryCollector() = runBlocking {
        val observer = NMPNip46Observer()
        val first = async(start = CoroutineStart.UNDISPATCHED) { observer.states.toList() }
        val second = async(start = CoroutineStart.UNDISPATCHED) { observer.states.toList() }

        observer.onReady("user-key")
        observer.onClosed()
        observer.onReady("must-not-follow-closed")

        val expected = listOf(
            NMPNip46ConnectionState.Ready("user-key"),
            NMPNip46ConnectionState.Closed,
        )
        assertEquals(expected, first.await())
        assertEquals(expected, second.await())
        assertEquals(
            listOf(NMPNip46ConnectionState.Closed),
            observer.states.toList(),
            "a late collector replays Closed and completes immediately",
        )
    }

    @Test
    fun publicConnectionCompletesChildHandshakeThenTimesOutAndCloses() = runBlocking {
        KotlinWebSocketBlackhole(holdHandshake = true).use { blackhole ->
            NMPEngine(NMPConfig()).use { engine ->
                val relay = URLEncoder.encode(blackhole.relay, StandardCharsets.UTF_8)
                val connection = engine.connectNip46(
                    "bunker://$remoteSignerPublicKey?relay=$relay&secret=public-runtime",
                    100.milliseconds,
                )
                val states = async(start = CoroutineStart.UNDISPATCHED) {
                    withTimeout(3.seconds) { connection.states.toList() }
                }

                blackhole.releaseHandshake()
                assertTrue(blackhole.awaitHandshake())
                val terminal = states.await()
                assertTrue(terminal.contains(NMPNip46ConnectionState.Available))
                assertEquals(
                    NMPNip46ConnectionState.Failed(NMPNip46Failure.Timeout),
                    terminal[terminal.lastIndex - 1],
                )
                assertEquals(NMPNip46ConnectionState.Closed, terminal.last())
                connection.close()
                connection.close()
            }
        }
    }

    @Test
    fun nativeResourcesCloseInstallationBeforeProviderAndOnlyOnce() {
        KotlinWebSocketBlackhole().use { blackhole ->
            val proof = compatibility()
            val projected = NMPNip46Observer()
            val closes = NativeCloseRecord()
            val forwarding = ForwardingObserver(projected, closes)
            NMPEngine(NMPConfig()).use { engine ->
                val prepared = prepare(blackhole, proof, forwarding)
                val installation = engine.installSignerProviderAdapter(prepared.adapter(proof))
                closes.setAction { installation.release() }
                val connection = NMPNip46Connection(projected, prepared, installation)

                assertTrue(blackhole.awaitHandshake())
                connection.close()
                connection.close()
                assertEquals(listOf(false), closes.awaitSnapshot())
            }
        }
    }

    @Test
    fun preparedAliasReplayCannotCloseOrInvalidateFirstInstallation() {
        KotlinWebSocketBlackhole().use { blackhole ->
            val proof = compatibility()
            val projected = NMPNip46Observer()
            val closes = NativeCloseRecord()
            val forwarding = ForwardingObserver(projected, closes)
            NMPEngine(NMPConfig()).use { engine ->
                val prepared = prepare(blackhole, proof, forwarding)
                val first = engine.installSignerProviderAdapter(prepared.adapter(proof))
                closes.setAction { first.release() }

                assertFailsWith<NMPProviderSignerInstallException.AdapterAlreadyTaken> {
                    engine.installSignerProviderAdapter(prepared.adapter(proof))
                }
                assertEquals(emptyList(), closes.snapshot())
                assertTrue(first.release())
                prepared.connection().use { it.disconnect() }
                assertEquals(listOf(false), closes.awaitSnapshot())
                prepared.close()
            }
        }
    }
}
