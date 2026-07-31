package com.nmp.sdk

import java.util.concurrent.atomic.AtomicReference
import uniffi.nmp_component_interface.FfiSignerAdapter
import uniffi.nmp_component_interface.nmpComponentInterfaceIdentity
import uniffi.nmp_ffi.FfiSignerAdapterInstallException
import uniffi.nmp_ffi.FfiSignerAdapterInstallation
import uniffi.nmp_ffi.nmpCoreComponentIdentity

/** Protocol-neutral boundary used only by separately packaged signer
 * providers. It consumes an opaque adapter, never exposing the core engine's
 * signer registration, driver, or reducer/runtime internals. */
@RequiresOptIn(
    level = RequiresOptIn.Level.ERROR,
    message = "This API is for NMP provider components, not app code.",
)
@Retention(AnnotationRetention.BINARY)
annotation class NMPProviderComponentApi

/** Exact identity embedded by the loaded core native component. Providers
 * compare this plain value before requesting an external Rust object. */
@NMPProviderComponentApi
fun nmpProviderCoreComponentIdentity(): String = nmpCoreComponentIdentity()

/** Identity of the core package's generated component-interface bindings. */
@NMPProviderComponentApi
fun nmpProviderComponentInterfaceIdentity(): String = nmpComponentInterfaceIdentity()

@NMPProviderComponentApi
fun NMPEngine.installSignerProviderAdapter(
    adapter: FfiSignerAdapter,
): NMPProviderSignerInstallation =
    try {
        NMPProviderSignerInstallation(ffi.installSignerAdapter(adapter))
    } catch (_: FfiSignerAdapterInstallException.EngineClosed) {
        throw NMPProviderSignerInstallException.EngineClosed()
    } catch (_: FfiSignerAdapterInstallException.AdapterAlreadyTaken) {
        throw NMPProviderSignerInstallException.AdapterAlreadyTaken()
    }

/** Typed refusal from the core-owned provider installation door. */
@NMPProviderComponentApi
sealed class NMPProviderSignerInstallException(message: String) : Exception(message) {
    class EngineClosed : NMPProviderSignerInstallException("core engine already shut down")
    class AdapterAlreadyTaken :
        NMPProviderSignerInstallException("provider signer adapter was already consumed")
}

/** Exact provider installation lease. Its nullable atomic reference is the
 * lifecycle: [close] consumes it once and repeats are inert. */
@NMPProviderComponentApi
class NMPProviderSignerInstallation internal constructor(
    installation: FfiSignerAdapterInstallation,
) : AutoCloseable {
    private val installation = AtomicReference<FfiSignerAdapterInstallation?>(installation)

    fun release(): Boolean {
        val current = installation.getAndSet(null) ?: return false
        val removed = current.uninstall()
        current.close()
        return removed
    }

    override fun close() {
        release()
    }
}
