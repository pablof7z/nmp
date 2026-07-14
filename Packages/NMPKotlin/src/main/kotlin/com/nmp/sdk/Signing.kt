package com.nmp.sdk

import uniffi.nmp_ffi.FfiSignEventRequest
import uniffi.nmp_ffi.FfiSignedEvent

data class NMPUnsignedEvent(
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
) {
    internal fun toFfi() = FfiSignEventRequest(createdAt, kind, tags, content)
}

data class NMPSignedEvent(
    val id: String,
    val pubkey: String,
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
    val sig: String,
) {
    internal constructor(event: FfiSignedEvent) : this(
        event.id,
        event.pubkey,
        event.createdAt,
        event.kind,
        event.tags,
        event.content,
        event.sig,
    )
}
