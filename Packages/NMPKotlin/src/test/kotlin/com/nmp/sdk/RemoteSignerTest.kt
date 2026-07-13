package com.nmp.sdk

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
            val amber = NMPLocalSignerDiscovery.known.single { it.id == "amber" }
            assertFailsWith<NMPError.InvalidSigner> { invitation.androidHandoff(amber) }
        }
    }
}
