package com.nmp.sdk

import uniffi.nmp_ffi.FfiCapabilityAvailability
import uniffi.nmp_ffi.FfiPrivateKey
import uniffi.nmp_ffi.FfiPublicKey
import uniffi.nmp_ffi.FfiSessionAccount
import uniffi.nmp_ffi.FfiSessionPayload
import uniffi.nmp_ffi.FfiSessionProviderKind
import uniffi.nmp_ffi.NmpEngine

/**
 * Opaque bytes representing one complete engine session.
 *
 * Store and restore the bytes as a whole. Account and provider restoration
 * material belongs to NMP and must not be parsed, partially edited, logged,
 * or rendered by the app.
 */
class NMPSessionPayload internal constructor(
    internal val ffi: FfiSessionPayload,
) {
    constructor(bytes: ByteArray) : this(FfiSessionPayload.fromBytes(bytes))

    /** A defensive copy of the opaque, sensitive transport bytes. */
    fun bytes(): ByteArray = ffi.bytes()

    override fun toString(): String = "NMPSessionPayload(<redacted>)"
}

/** A decoded Nostr public key. Bech32 exists only at the app's human boundary. */
class NMPPublicKey private constructor(
    internal val ffi: FfiPublicKey,
) {
    constructor(bytes: ByteArray) : this(
        nmpRethrowing { FfiPublicKey.fromBytes(bytes.copyOf()) },
    )

    /** A defensive copy of the exact decoded 32-byte key. */
    val bytes: ByteArray
        get() = ffi.bytes()

    override fun equals(other: Any?): Boolean =
        other is NMPPublicKey && bytes.contentEquals(other.bytes)

    override fun hashCode(): Int = bytes.contentHashCode()

    override fun toString(): String = "NMPPublicKey"

    internal companion object {
        fun from(ffi: FfiPublicKey): NMPPublicKey = NMPPublicKey(ffi)
    }
}

/**
 * A decoded private key handed directly to NMP's provider boundary.
 *
 * It deliberately has no byte accessor and its string representation is
 * redacted. NMP owns the provider after account addition.
 */
class NMPPrivateKey private constructor(
    internal val ffi: FfiPrivateKey,
) {
    constructor(bytes: ByteArray) : this(
        nmpRethrowing { FfiPrivateKey.fromBytes(bytes) },
    )

    override fun toString(): String = "NMPPrivateKey(<redacted>)"

    companion object {
        /** Generate one decoded private key inside NMP's native boundary. */
        fun generate(): NMPPrivateKey = NMPPrivateKey(FfiPrivateKey.generate())
    }
}

/** The configured provider kind, independent of its current availability. */
enum class NMPSessionProviderKind {
    LocalKey,
    ;

    internal companion object {
        fun from(ffi: FfiSessionProviderKind): NMPSessionProviderKind =
            when (ffi) {
                FfiSessionProviderKind.LOCAL_KEY -> LocalKey
            }
    }
}

/** The current operational state of one cryptographic capability. */
sealed interface NMPCapabilityAvailability {
    data object Unsupported : NMPCapabilityAvailability

    data object Available : NMPCapabilityAvailability

    data class Unavailable(val reason: String) : NMPCapabilityAvailability

    companion object {
        internal fun from(ffi: FfiCapabilityAvailability): NMPCapabilityAvailability =
            when (ffi) {
                is FfiCapabilityAvailability.Unsupported -> Unsupported
                is FfiCapabilityAvailability.Available -> Available
                is FfiCapabilityAvailability.Unavailable -> Unavailable(ffi.reason)
            }
    }
}

/**
 * Opaque identity for one exact account in the current engine session.
 *
 * Signer-backed and public-key-only accounts use this same handle. Removing
 * it removes the whole account, including its provider. Repeated removal is a
 * no-op; adding the same public key again refers to that same account identity.
 */
class NMPSessionAccount internal constructor(
    internal val ffi: FfiSessionAccount,
) {
    val publicKey: NMPPublicKey = NMPPublicKey.from(ffi.publicKey())
    val providerKind: NMPSessionProviderKind? =
        ffi.provider()?.let(NMPSessionProviderKind::from)
    val signingAvailability: NMPCapabilityAvailability =
        NMPCapabilityAvailability.from(ffi.signingAvailability())

    override fun toString(): String = "NMPSessionAccount"
}

/**
 * The engine-owned view of one complete account session.
 *
 * This object owns no second runtime. All mutations go through the same
 * engine transition, and no secret or restoration material is retained in
 * this Kotlin wrapper.
 */
class NMPSession internal constructor(
    private val ffi: NmpEngine,
) {
    val accounts: List<NMPSessionAccount>
        get() = nmpRethrowing { ffi.session().accounts.map(::NMPSessionAccount) }

    val current: NMPSessionAccount?
        get() =
            nmpRethrowing {
                val snapshot = ffi.session()
                val currentPublicKey = snapshot.currentPublicKey ?: return@nmpRethrowing null
                snapshot.accounts
                    .firstOrNull {
                        it.publicKey().bytes().contentEquals(currentPublicKey.bytes())
                    }
                    ?.let(::NMPSessionAccount)
            }

    /** Add a private-key-backed account, optionally selecting it atomically. */
    fun add(
        privateKey: NMPPrivateKey,
        makeCurrent: Boolean = false,
    ): NMPSessionAccount =
        nmpRethrowing {
            NMPSessionAccount(ffi.addPrivateKeyAccount(privateKey.ffi, makeCurrent))
        }

    /**
     * Add an ordinary account without a signer provider.
     *
     * Decode bech32 at the app's human input boundary before constructing the
     * decoded [NMPPublicKey] passed here.
     */
    fun add(
        publicKey: NMPPublicKey,
        makeCurrent: Boolean = false,
    ): NMPSessionAccount =
        nmpRethrowing {
            NMPSessionAccount(ffi.addPublicKeyAccount(publicKey.ffi, makeCurrent))
        }

    fun makeCurrent(account: NMPSessionAccount) =
        nmpRethrowing { ffi.makeCurrentAccount(account.ffi) }

    /** Remove this whole account. Repeated removal is a no-op. */
    fun remove(account: NMPSessionAccount): Boolean =
        nmpRethrowing { ffi.removeAccount(account.ffi) }

    /**
     * Clear accounts, providers, and current selection without clearing
     * cached events, receipts, or accepted write obligations.
     */
    fun clear() = nmpRethrowing { ffi.clearSession() }

    /** Export the one complete opaque value for atomic app storage. */
    fun export(): NMPSessionPayload =
        nmpRethrowing { NMPSessionPayload(ffi.exportSession()) }
}
