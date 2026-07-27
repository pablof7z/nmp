package com.nmp.sdk

import uniffi.nmp_ffi.FfiSignerMailbox
import uniffi.nmp_ffi.nmpCoreComponentIdentity

/** Protocol-neutral boundary used only by separately packaged signer
 * providers. It exposes an opaque mailbox, never the core engine or its
 * reducer/runtime internals. */
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

@NMPProviderComponentApi
fun NMPEngine.signerProviderMailbox(): FfiSignerMailbox = ffi.signerMailbox()
