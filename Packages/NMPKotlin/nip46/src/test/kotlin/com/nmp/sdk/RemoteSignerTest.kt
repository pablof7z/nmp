package com.nmp.sdk

import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.flow.toList
import kotlinx.coroutines.runBlocking
import java.util.concurrent.atomic.AtomicInteger
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class RemoteSignerTest {
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
    fun mismatchedCoreIsTypedBeforeTheMailboxBodyRuns() {
        var mailboxBodyRan = false

        val error = assertFailsWith<NMPError.NativeComponentMismatch> {
            withVerifiedNip46Core("deliberately-mismatched-core") {
                mailboxBodyRan = true
            }
        }
        assertEquals("nmp-nip46", error.component)
        assertTrue(error.expectedCoreIdentity.startsWith("nmp-core-component-v1-"))
        assertEquals("deliberately-mismatched-core", error.actualCoreIdentity)
        assertFalse(mailboxBodyRan)
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
