package com.nmp.sdk

import kotlinx.coroutines.channels.BufferOverflow
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.MutableSharedFlow
import kotlinx.coroutines.flow.transformWhile
import java.util.concurrent.atomic.AtomicBoolean
import kotlin.time.Duration
import kotlin.time.Duration.Companion.seconds
import uniffi.nmp_ffi.FfiBunkerParseError
import uniffi.nmp_ffi.FfiLocalSignerApp
import uniffi.nmp_ffi.FfiLocalSignerProtocol
import uniffi.nmp_ffi.FfiNip46ClientMetadata
import uniffi.nmp_ffi.FfiNip46ConnectionEvent
import uniffi.nmp_ffi.FfiNip46Failure
import uniffi.nmp_ffi.FfiNip46Invitation
import uniffi.nmp_ffi.Nip46Connection
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

/** `nmp_signer::BunkerParseError` mirror (mirrors `nmp-ffi`'s own
 * `FfiBunkerParseError`; see that type's doc for the Rust side of each
 * case). */
sealed interface NMPBunkerParseFailure {
    data object Empty : NMPBunkerParseFailure
    data class TooLong(val len: ULong) : NMPBunkerParseFailure
    data object WrongScheme : NMPBunkerParseFailure
    data object MissingRemoteSignerKey : NMPBunkerParseFailure
    data object InvalidRemoteSignerKey : NMPBunkerParseFailure
    data object MissingRelay : NMPBunkerParseFailure
    data class TooManyRelays(val count: ULong) : NMPBunkerParseFailure
    data class InvalidRelay(val relay: String) : NMPBunkerParseFailure
    data class Malformed(val reason: String) : NMPBunkerParseFailure

    companion object {
        internal fun from(ffi: FfiBunkerParseError): NMPBunkerParseFailure =
            when (ffi) {
                FfiBunkerParseError.Empty -> Empty
                is FfiBunkerParseError.TooLong -> TooLong(ffi.len)
                FfiBunkerParseError.WrongScheme -> WrongScheme
                FfiBunkerParseError.MissingRemoteSignerKey -> MissingRemoteSignerKey
                FfiBunkerParseError.InvalidRemoteSignerKey -> InvalidRemoteSignerKey
                FfiBunkerParseError.MissingRelay -> MissingRelay
                is FfiBunkerParseError.TooManyRelays -> TooManyRelays(ffi.count)
                is FfiBunkerParseError.InvalidRelay -> InvalidRelay(ffi.relay)
                is FfiBunkerParseError.Malformed -> Malformed(ffi.reason)
            }
    }
}

/** Typed NIP-46 connection failure (mirrors `nmp-ffi`'s own
 * `FfiNip46Failure`; see that type's doc for the Rust side of each case). */
sealed interface NMPNip46Failure {
    data class InvalidBunkerUri(val source: NMPBunkerParseFailure) : NMPNip46Failure
    data object MissingRelay : NMPNip46Failure
    data class TooManyRelays(val count: ULong) : NMPNip46Failure
    data class InvitationTooLong(val len: ULong) : NMPNip46Failure
    data class InvalidLaunchScheme(val scheme: String) : NMPNip46Failure
    data object Timeout : NMPNip46Failure
    data object Disconnected : NMPNip46Failure
    data class Rejected(val reason: String) : NMPNip46Failure
    data class InvalidResponse(val reason: String) : NMPNip46Failure
    data object SignerMissingPublicKey : NMPNip46Failure

    /** A restore/import's live answer did not match the checkpoint's
     * expected identity (#571). No signer was attached under the wrong
     * pubkey. */
    data class RestoredIdentityMismatch(val expected: String, val actual: String) : NMPNip46Failure

    companion object {
        internal fun from(ffi: FfiNip46Failure): NMPNip46Failure =
            when (ffi) {
                is FfiNip46Failure.InvalidBunkerUri ->
                    InvalidBunkerUri(NMPBunkerParseFailure.from(ffi.source))
                FfiNip46Failure.MissingRelay -> MissingRelay
                is FfiNip46Failure.TooManyRelays -> TooManyRelays(ffi.count)
                is FfiNip46Failure.InvitationTooLong -> InvitationTooLong(ffi.len)
                is FfiNip46Failure.InvalidLaunchScheme -> InvalidLaunchScheme(ffi.scheme)
                FfiNip46Failure.Timeout -> Timeout
                FfiNip46Failure.Disconnected -> Disconnected
                is FfiNip46Failure.Rejected -> Rejected(ffi.reason)
                is FfiNip46Failure.InvalidResponse -> InvalidResponse(ffi.reason)
                FfiNip46Failure.SignerMissingPublicKey -> SignerMissingPublicKey
                is FfiNip46Failure.RestoredIdentityMismatch ->
                    RestoredIdentityMismatch(ffi.expected, ffi.actual)
            }
    }
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
    data class Failed(val failure: NMPNip46Failure) : NMPNip46ConnectionState
    object Closed : NMPNip46ConnectionState
}

internal class NMPNip46Observer : Nip46ConnectionObserver {
    private val lock = Any()
    private var closed = false
    private val mutableStates = MutableSharedFlow<NMPNip46ConnectionState>(
        replay = 1,
        extraBufferCapacity = 31,
        onBufferOverflow = BufferOverflow.DROP_OLDEST,
    )
    val states: Flow<NMPNip46ConnectionState> = mutableStates.transformWhile { state ->
        emit(state)
        state !is NMPNip46ConnectionState.Closed
    }

    private fun emitIfOpen(state: NMPNip46ConnectionState) {
        synchronized(lock) {
            if (!closed) {
                mutableStates.tryEmit(state)
            }
        }
    }

    override fun onEvent(event: FfiNip46ConnectionEvent) {
        emitIfOpen(
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
        emitIfOpen(NMPNip46ConnectionState.Ready(userPublicKey))
    }

    override fun onFailed(failure: FfiNip46Failure) {
        emitIfOpen(NMPNip46ConnectionState.Failed(NMPNip46Failure.from(failure)))
    }

    override fun onClosed() {
        synchronized(lock) {
            if (!closed) {
                closed = true
                mutableStates.tryEmit(NMPNip46ConnectionState.Closed)
            }
        }
    }
}

class NMPNip46Connection internal constructor(
    internal val observer: NMPNip46Observer,
    private val ffiConnection: Nip46Connection?,
    private val disconnect: () -> Unit,
) : AutoCloseable {
    internal constructor(observer: NMPNip46Observer, disconnect: () -> Unit) : this(
        observer,
        null,
        disconnect,
    )

    internal constructor(observer: NMPNip46Observer, ffiConnection: Nip46Connection) : this(
        observer,
        ffiConnection,
        ffiConnection::disconnect,
    )

    private val closed = AtomicBoolean(false)
    val states: Flow<NMPNip46ConnectionState> = observer.states

    /** Read out this session's checkpoint (#571): the minimum secrets and
     * descriptor needed to reconnect without another pairing handshake --
     * see [NMPNip46SessionCheckpoint]'s doc. Refused with a typed error
     * before this connection has reached `Ready`; checkpointing a session
     * that never authenticated would persist meaningless material. */
    fun checkpoint(): NMPNip46SessionCheckpoint {
        val connection = ffiConnection
            ?: throw NMPError.InvalidSigner("no underlying NIP-46 connection to checkpoint")
        return NMPNip46SessionCheckpoint(nmpRethrowing { connection.checkpoint() })
    }

    /** Idempotently detach this exact signer session and emit [NMPNip46ConnectionState.Closed]. */
    override fun close() {
        if (closed.compareAndSet(false, true)) {
            disconnect()
        }
    }
}

class NMPNip46Invitation internal constructor(internal val ffi: FfiNip46Invitation) {
    /** Generic chooser URI, or an app-specific URI when [signer] is supplied. */
    fun uri(signer: NMPLocalSigner? = null): String =
        nmpRethrowing { ffi.uri(signer?.id) }

    /** Exact Android one-click handoff. The host launches [uri] with
     * `Intent.setPackage(packageName)`; OS acceptance is not signer readiness,
     * which is reported later as [NMPNip46ConnectionState.Ready]. */
    fun androidHandoff(signer: NMPLocalSigner): NMPAndroidSignerHandoff {
        val canonical = NMPLocalSignerDiscovery.known.singleOrNull { it.id == signer.id }
            ?: throw NMPError.InvalidSigner("unknown local signer id ${signer.id}")
        if (NMPLocalSignerProtocol.Nip46 !in canonical.protocols) {
            throw NMPError.InvalidSigner("${canonical.displayName} does not support NIP-46")
        }
        val packageName = canonical.androidPackageId
            ?: throw NMPError.InvalidSigner("${canonical.displayName} has no Android package")
        return NMPAndroidSignerHandoff(uri = uri(canonical), packageName = packageName)
    }
}

data class NMPAndroidSignerHandoff(val uri: String, val packageName: String)

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
): NMPNip46Connection {
    val observer = NMPNip46Observer()
    val ffiConnection = nmpRethrowing {
        ffi.connectNip46Bunker(
            bunkerUri,
            timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
            observer,
        )
    }
    return NMPNip46Connection(observer, ffiConnection)
}

fun NMPEngine.connectNip46(
    invitation: NMPNip46Invitation,
    timeout: Duration = 60.seconds,
): NMPNip46Connection {
    val observer = NMPNip46Observer()
    val ffiConnection = nmpRethrowing {
        ffi.connectNip46Invitation(
            invitation.ffi,
            timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
            observer,
        )
    }
    return NMPNip46Connection(observer, ffiConnection)
}

/** Restore an already-authorized NIP-46 client session from [store]'s
 * checkpoint (#571) -- reconnects the SAME client transport identity to the
 * SAME remote signer with NO re-pairing handshake. Returns `null` without
 * connecting anything when [store] holds no checkpoint. As with
 * [connectNip46], [NMPNip46ConnectionState.Ready] fires only once the
 * checkpoint's expected identity is validated against a live answer and the
 * signer is attached to this engine; a mismatch/corrupt/unavailable outcome
 * surfaces as a typed [NMPNip46ConnectionState.Failed], never a thrown
 * exception from this call. */
fun NMPEngine.restoreNip46Session(
    store: NMPNip46SessionCheckpointStore,
    timeout: Duration = 60.seconds,
): NMPNip46Connection? {
    val checkpoint = store.loadCheckpoint() ?: return null
    return restoreNip46Session(checkpoint, timeout)
}

/** Reconnect from an explicit checkpoint value with no store involved --
 * the primitive [restoreNip46Session] (store overload) builds on directly. */
fun NMPEngine.restoreNip46Session(
    checkpoint: NMPNip46SessionCheckpoint,
    timeout: Duration = 60.seconds,
): NMPNip46Connection {
    val observer = NMPNip46Observer()
    val ffiConnection = nmpRethrowing {
        ffi.restoreNip46Session(
            checkpoint.toFfi(),
            timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
            observer,
        )
    }
    return NMPNip46Connection(observer, ffiConnection)
}

/** Brownfield migration door (#571): import a pre-NMP legacy client session
 * (for example Pod0's securely-persisted `nostrconnect://` material)
 * directly from its raw parts, without first constructing an NMP-owned
 * checkpoint or ever writing one to a store. Validates
 * [expectedUserPublicKey] before `Ready` exactly like [restoreNip46Session],
 * and never deletes or overwrites the caller's legacy material -- a
 * mismatch/corrupt import surfaces only as a typed
 * [NMPNip46ConnectionState.Failed], never by touching [clientSecretKey]'s
 * original source. */
fun NMPEngine.importNip46Session(
    clientSecretKey: String,
    expectedUserPublicKey: String,
    remoteSignerPublicKey: String,
    relays: List<String>,
    origin: NMPNip46SessionOrigin,
    timeout: Duration = 60.seconds,
): NMPNip46Connection {
    val parts = NMPNip46SessionCheckpoint(
        clientSecretKey = clientSecretKey,
        userPublicKey = expectedUserPublicKey,
        remoteSignerPublicKey = remoteSignerPublicKey,
        relays = relays,
        origin = origin,
    )
    val observer = NMPNip46Observer()
    val ffiConnection = nmpRethrowing {
        ffi.nip46SessionFromParts(
            parts.toFfi(),
            timeout.inWholeMilliseconds.coerceAtLeast(0).toULong(),
            observer,
        )
    }
    return NMPNip46Connection(observer, ffiConnection)
}
