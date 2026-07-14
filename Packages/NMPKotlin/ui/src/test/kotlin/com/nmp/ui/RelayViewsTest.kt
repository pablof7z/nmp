package com.nmp.ui

import androidx.compose.runtime.Composable
import androidx.compose.ui.graphics.painter.Painter
import com.nmp.sdk.AuthPhase
import com.nmp.sdk.RelayInformationFreshness
import com.nmp.sdk.SourceStatus
import java.io.File
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFalse
import kotlin.test.assertNotNull
import kotlin.test.assertNull

class RelayViewsTest {
    @Test
    fun relayPresentationTrimsAdvertisedFieldsAndFallsBackDeterministically() {
        val advertised =
            NmpRelayPresentation(
                relay = "wss://relay.example",
                advertisedName = "  Example Relay  ",
                advertisedDescription = "  A useful relay  ",
                advertisedIcon = "https://media.example/icon.png",
                freshness = RelayInformationFreshness.Fresh,
            )
        assertEquals("Example Relay", advertised.displayName)
        assertEquals("A useful relay", advertised.displayDescription)
        assertEquals("ER", advertised.initials)

        val fallback =
            NmpRelayPresentation(
                relay = "wss://fallback.example:443/path",
                advertisedName = "  ",
                freshness = RelayInformationFreshness.Fresh,
            )
        assertEquals("fallback.example", fallback.displayName)
        assertEquals("No relay description provided", fallback.displayDescription)
        assertEquals("FA", fallback.initials)
    }

    @Test
    fun freshStaleLastGoodAndUnavailableRemainDistinct() {
        val fresh =
            NmpRelayInformationState.Available(
                NmpRelayPresentation(
                    relay = "wss://fresh.example",
                    advertisedName = "Fresh",
                    freshness = RelayInformationFreshness.Fresh,
                ),
            )
        val stale =
            NmpRelayInformationState.Available(
                NmpRelayPresentation(
                    relay = "wss://stale.example",
                    advertisedName = "Last good name",
                    advertisedDescription = "Last good description",
                    freshness = RelayInformationFreshness.Stale,
                    lastError = "refresh timed out",
                ),
            )
        val unavailable =
            NmpRelayInformationState.Unavailable(
                relay = "wss://missing.example",
                reason = "document unavailable",
            )

        assertEquals("Fresh", fresh.informationLabel)
        assertNull(fresh.lastError)
        assertEquals("Stale", stale.informationLabel)
        assertEquals("Last good name", stale.displayName)
        assertEquals("Last good description", stale.displayDescription)
        assertEquals("refresh timed out", stale.lastError)
        assertEquals("Unavailable", unavailable.informationLabel)
        assertEquals("missing.example", unavailable.displayName)
        assertEquals("document unavailable", unavailable.displayDescription)
    }

    @Test
    fun everyProjectedSourceStatusMapsWithoutInventingGlobalHealth() {
        val cases =
            listOf(
                Triple(null, NmpRelayRuntimePresentation.StatusUnavailable, "Status unavailable"),
                Triple(SourceStatus.Requesting, NmpRelayRuntimePresentation.Requesting, "Requesting"),
                Triple(SourceStatus.Connecting, NmpRelayRuntimePresentation.Connecting, "Connecting"),
                Triple(SourceStatus.Disconnected, NmpRelayRuntimePresentation.Disconnected, "Disconnected"),
                Triple(
                    SourceStatus.AwaitingAuth(AuthPhase.AwaitingPolicy),
                    NmpRelayRuntimePresentation.AwaitingAuth(AuthPhase.AwaitingPolicy),
                    "Awaiting authentication policy",
                ),
                Triple(
                    SourceStatus.AwaitingAuth(AuthPhase.AwaitingSignature),
                    NmpRelayRuntimePresentation.AwaitingAuth(AuthPhase.AwaitingSignature),
                    "Awaiting authentication signature",
                ),
                Triple(SourceStatus.AuthDenied, NmpRelayRuntimePresentation.AuthDenied, "Authentication denied"),
                Triple(SourceStatus.Error, NmpRelayRuntimePresentation.Error, "Connection error"),
            )

        cases.forEach { (source, expected, label) ->
            val projected = NmpRelayRuntimePresentation.from(source)
            assertEquals(expected, projected)
            assertEquals(label, projected.label)
        }
    }

    @Test
    fun everyControlledComposePrimitiveConstructsWithoutAnEngineOrLoader() {
        val information =
            NmpRelayInformationState.Available(
                NmpRelayPresentation(
                    relay = "wss://relay.example",
                    advertisedName = "Relay",
                    advertisedDescription = "Description",
                    advertisedIcon = "https://media.example/icon.png",
                    freshness = RelayInformationFreshness.Stale,
                    lastError = "refresh failed",
                ),
            )
        val painter: Painter? = null
        val content: @Composable () -> Unit = {
            NmpRelayIcon(state = information, painter = painter)
            NmpRelayName(state = information)
            NmpRelayDescription(state = information)
            NmpRelayRuntimeStatus(status = NmpRelayRuntimePresentation.Connecting)
            NmpRelayListEntry(
                information = information,
                runtime = NmpRelayRuntimePresentation.Connecting,
                painter = painter,
                onClick = {},
            )
        }
        assertNotNull(content)
    }

    @Test
    fun relayViewsAndCoreSdkHaveNoAcquisitionOrComposeDependencyLeak() {
        val working = File(System.getProperty("user.dir"))
        val uiSource =
            sequenceOf(
                File(working, "src/main/kotlin/com/nmp/ui/RelayViews.kt"),
                File(working, "ui/src/main/kotlin/com/nmp/ui/RelayViews.kt"),
            ).first { it.isFile }
        val source = uiSource.readText()
        listOf(
            "NMPEngine",
            "NmpEngine",
            "relayInformation(",
            "HttpClient",
            "URLSession",
            "AsyncImage",
            "Timer",
            "delay(",
        ).forEach { forbidden ->
            assertFalse(source.contains(forbidden), "Relay views must not contain $forbidden")
        }

        val kotlinRoot =
            generateSequence(working) { it.parentFile }
                .first {
                    File(it, "settings.gradle.kts").isFile &&
                        File(it, "src/main/kotlin/com/nmp/sdk").isDirectory
                }
        val coreBuild = File(kotlinRoot, "build.gradle.kts")
        val coreScript = coreBuild.readText()
        assertFalse(coreScript.contains("org.jetbrains.compose"))
        assertFalse(coreScript.contains("compose.desktop"))
        assertFalse(coreScript.contains("compose.material"))
    }
}
