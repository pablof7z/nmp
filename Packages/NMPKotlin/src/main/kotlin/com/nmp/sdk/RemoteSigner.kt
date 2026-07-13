package com.nmp.sdk

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds
import uniffi.nmp_ffi.FfiLocalSignerApp
import uniffi.nmp_ffi.FfiLocalSignerProtocol
import uniffi.nmp_ffi.FfiNip46ClientMetadata
import uniffi.nmp_ffi.FfiNip46ConnectionEvent
import uniffi.nmp_ffi.FfiNip46Invitation
import uniffi.nmp_ffi.Nip46ConnectionObserver
import uniffi.nmp_ffi.localSignerCatalog

enum class NMPLocalSignerProtocol { Nip46, Nip55 }

/** Rust-owned local-signer facts. Android code should query the exact
 * [androidDetectionUri], filter handlers by [androidPackageId], then launch
 * the Rust-generated handoff URI. A shared `nostrsigner:` scheme is never an
 * app identity. */
data class NMPLocalSigner(
    val id: String,
    val displayName: String,
    val protocols: Set<NMPLocalSignerProtocol>,
    val iosDetectionUri: String?,
    val nip46LaunchScheme: String?,
    val androidDetectionUri: String?,
    val androidPackageId: String?,
    val androidProviderAuthority: String?,
) {
    internal constructor(ffi: FfiLocalSignerApp) : this(
        id = ffi.id,
        displayName = ffi.displayName,
        protocols = ffi.protocols.mapTo(mutableSetOf()) {
            when (it) {
                FfiLocalSignerProtocol.NIP46 -> NMPLocalSignerProtocol.Nip46
                FfiLocalSignerProtocol.NIP55 -> NMPLocalSignerProtocol.Nip55
            }
        },
        iosDetectionUri = ffi.iosDetectionUri,
        nip46LaunchScheme = ffi.nip46LaunchScheme,
        androidDetectionUri = ffi.androidDetectionUri,
        androidPackageId = ffi.androidPackageId,
        androidProviderAuthority = ffi.androidProviderAuthority,
    )
}

object NMPLocalSignerDiscovery {
    val known: List<NMPLocalSigner>
        get() = localSignerCatalog().map(::NMPLocalSigner)

    /** Pure package-filtered projection for an Android host that has already
     * executed PackageManager queries and reports raw package IDs. */
    fun installedAndroid(packageIds: Set<String>): List<NMPLocalSigner> =
        known.filter { signer -> signer.androidPackageId in packageIds }
}

data class NMPNip46ClientMetadata(
    val name: String? = null,
    val url: String? = null,
    val image: String? = null,
) {
    internal fun toFfi() = FfiNip46ClientMetadata(name, url, image)
}

sealed interface NMPNip46ConnectionState {
    object Connecting : NMPNip46ConnectionState
    object Available : NMPNip46ConnectionState
    object Unavailable : NMPNip46ConnectionState
    data class RelayAuthentication(val relay: String) : NMPNip46ConnectionState
    data class AuthorizationRequired(val url: String) : NMPNip46ConnectionState
    data class Connected(val userPublicKey: String) : NMPNip46ConnectionState
    /** Stronger than [Connected]: the signer is attached to this engine. */
    data class Ready(val userPublicKey: String) : NMPNip46ConnectionState
    data class Failed(val reason: String) : NMPNip46ConnectionState
}

internal class NMPNip46Observer : Nip46ConnectionObserver {
    private val channel = Channel<NMPNip46ConnectionState>(Channel.BUFFERED)
    val states: Flow<NMPNip46ConnectionState> = channel.receiveAsFlow()

    override fun onEvent(event: FfiNip46ConnectionEvent) {
        channel.trySend(
            when (event) {
                FfiNip46ConnectionEvent.Connecting -> NMPNip46ConnectionState.Connecting
                FfiNip46ConnectionEvent.Available -> NMPNip46ConnectionState.Available
                FfiNip46ConnectionEvent.Unavailable -> NMPNip46ConnectionState.Unavailable
                is FfiNip46ConnectionEvent.RelayAuthentication ->
                    NMPNip46ConnectionState.RelayAuthentication(event.relay)
                is FfiNip46ConnectionEvent.AuthorizationRequired ->
                    NMPNip46ConnectionState.AuthorizationRequired(event.url)
                is FfiNip46ConnectionEvent.Connected ->
                    NMPNip46ConnectionState.Connected(event.userPublicKey)
            },
        )
    }

    override fun onReady(userPublicKey: String) {
        channel.trySend(NMPNip46ConnectionState.Ready(userPublicKey))
    }

    override fun onFailed(reason: String) {
        channel.trySend(NMPNip46ConnectionState.Failed(reason))
        channel.close()
    }
}

class NMPNip46Connection internal constructor(
    internal val observer: NMPNip46Observer = NMPNip46Observer(),
) {
    val states: Flow<NMPNip46ConnectionState> = observer.states
}

class NMPNip46Invitation internal constructor(internal val ffi: FfiNip46Invitation) {
    /** Generic chooser URI, or an app-specific URI when [signer] is supplied. */
    fun uri(signer: NMPLocalSigner? = null): String =
        nmpRethrowing { ffi.uri(signer?.id) }
}

fun NMPEngine.nip46Invitation(
    relays: List<String>,
    permissions: String? = null,
    metadata: NMPNip46ClientMetadata = NMPNip46ClientMetadata(),
): NMPNip46Invitation = NMPNip46Invitation(
    nmpRethrowing { ffi.nip46Invitation(relays, permissions, metadata.toFfi()) },
)

fun NMPEngine.connectNip46(
    bunkerUri: String,
    timeout: Duration = 60.seconds,
): NMPNip46Connection = NMPNip46Connection().also { connection ->
    ffi.connectNip46Bunker(
        bunkerUri,
        timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
        connection.observer,
    )
}

fun NMPEngine.connectNip46(
    invitation: NMPNip46Invitation,
    timeout: Duration = 60.seconds,
): NMPNip46Connection = NMPNip46Connection().also { connection ->
    nmpRethrowing {
        ffi.connectNip46Invitation(
            invitation.ffi,
            timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
            connection.observer,
        )
    }
}
