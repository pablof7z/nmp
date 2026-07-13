package com.nmp.sdk

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.take
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertNull
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class RemoteSignerTest {
    @Test
    fun catalogSeparatesDetectionLaunchPackageAndProviderFacts() {
        val primal = NMPLocalSignerDiscovery.known.single { it.id == "primal" }
        assertEquals("primalconnect://probe", primal.iosDetectionUri)
        assertEquals("primalconnect", primal.nip46LaunchScheme)
        assertEquals("primal://signer", primal.androidDetectionUri)
        assertEquals("net.primal.android", primal.androidPackageId)
        assertEquals("net.primal.android", primal.androidProviderAuthority)
        assertEquals(setOf(NMPLocalSignerProtocol.Nip46, NMPLocalSignerProtocol.Nip55), primal.protocols)

        val amber = NMPLocalSignerDiscovery.known.single { it.id == "amber" }
        assertNull(amber.iosDetectionUri)
        assertEquals(setOf(NMPLocalSignerProtocol.Nip55), amber.protocols)
    }

    @Test
    fun androidDiscoveryIsPackageFilteredWhenSchemesAreShared() {
        assertEquals(
            listOf("amber"),
            NMPLocalSignerDiscovery.installedAndroid(setOf("com.greenart7c3.nostrsigner")).map { it.id },
        )
        assertEquals(
            listOf("primal"),
            NMPLocalSignerDiscovery.installedAndroid(setOf("net.primal.android")).map { it.id },
        )
        assertEquals(
            listOf("primal", "amber"),
            NMPLocalSignerDiscovery.installedAndroid(
                setOf("net.primal.android", "com.greenart7c3.nostrsigner"),
            ).map { it.id },
        )
    }

    @Test
    fun primalInvitationChangesOnlyTheLaunchScheme() {
        NMPEngine(NMPConfig()).use { engine ->
            val invitation = engine.nip46Invitation(listOf("wss://relay.example"))
            val generic = invitation.uri()
            val primal = NMPLocalSignerDiscovery.known.single { it.id == "primal" }
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
                protocols = setOf(NMPLocalSignerProtocol.Nip46),
                androidPackageId = "attacker.example",
            )
            assertEquals(
                "net.primal.android",
                invitation.androidHandoff(forged).packageName,
                "handoff must resolve package and protocol from the Rust catalog by id",
            )
            val amber = NMPLocalSignerDiscovery.known.single { it.id == "amber" }
            assertFailsWith<NMPError.InvalidSigner> { invitation.androidHandoff(amber) }
        }
    }

    @Test
    fun connectionStatesMulticastReadyAndClosedToEveryCollector() = runBlocking {
        val observer = NMPNip46Observer()
        val first = async(start = CoroutineStart.UNDISPATCHED) { observer.states.take(2).toList() }
        val second = async(start = CoroutineStart.UNDISPATCHED) { observer.states.take(2).toList() }

        observer.onReady("user-key")
        observer.onClosed()

        val expected = listOf(
            NMPNip46ConnectionState.Ready("user-key"),
            NMPNip46ConnectionState.Closed,
        )
        assertEquals(expected, first.await())
        assertEquals(expected, second.await())
    }

    @Test
    fun nativeConnectionCloseIsIdempotentAndScopedToItsOwnHandle() {
        val closedA = AtomicInteger(0)
        val closedB = AtomicInteger(0)
        val connectionA = NMPNip46Connection(NMPNip46Observer()) { closedA.incrementAndGet() }
        val connectionB = NMPNip46Connection(NMPNip46Observer()) { closedB.incrementAndGet() }

        connectionA.close()
        connectionA.close()
        assertEquals(1, closedA.get())
        assertEquals(0, closedB.get())

        connectionB.close()
        assertEquals(1, closedB.get())
    }
}
