package com.nmp.sdk

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import uniffi.nmp_ffi.FfiRelayListActionFailure
import uniffi.nmp_ffi.FfiRelayListActionStatus
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.RelayListActionObserver

sealed interface RelayListActionFailure {
    data class InvalidRelay(val got: String) : RelayListActionFailure
    data object SignedOut : RelayListActionFailure
    data object AccountChanged : RelayListActionFailure
    data object AcquisitionTimedOut : RelayListActionFailure
    data object CachedOnly : RelayListActionFailure
    data object SourceUnavailable : RelayListActionFailure
    data object BaseHasWrongAuthor : RelayListActionFailure
    data object BaseHasWrongKind : RelayListActionFailure
    data object TimestampExhausted : RelayListActionFailure
    data object InvalidGeneratedTag : RelayListActionFailure
    data object EngineClosed : RelayListActionFailure
    data object ReceiptUnavailable : RelayListActionFailure
    data class ThreadUnavailable(val component: String, val reason: String) : RelayListActionFailure
    data class ExecutorSaturated(val component: String, val capacity: ULong) : RelayListActionFailure

    companion object {
        fun from(ffi: FfiRelayListActionFailure): RelayListActionFailure =
            when (ffi) {
                is FfiRelayListActionFailure.InvalidRelay -> InvalidRelay(ffi.got)
                is FfiRelayListActionFailure.SignedOut -> SignedOut
                is FfiRelayListActionFailure.AccountChanged -> AccountChanged
                is FfiRelayListActionFailure.AcquisitionTimedOut -> AcquisitionTimedOut
                is FfiRelayListActionFailure.CachedOnly -> CachedOnly
                is FfiRelayListActionFailure.SourceUnavailable -> SourceUnavailable
                is FfiRelayListActionFailure.BaseHasWrongAuthor -> BaseHasWrongAuthor
                is FfiRelayListActionFailure.BaseHasWrongKind -> BaseHasWrongKind
                is FfiRelayListActionFailure.TimestampExhausted -> TimestampExhausted
                is FfiRelayListActionFailure.InvalidGeneratedTag -> InvalidGeneratedTag
                is FfiRelayListActionFailure.EngineClosed -> EngineClosed
                is FfiRelayListActionFailure.ReceiptUnavailable -> ReceiptUnavailable
                is FfiRelayListActionFailure.ThreadUnavailable ->
                    ThreadUnavailable(ffi.component, ffi.reason)
                is FfiRelayListActionFailure.ExecutorSaturated ->
                    ExecutorSaturated(ffi.component, ffi.capacity)
            }
    }
}

sealed interface RelayListActionStatus {
    data object Acquiring : RelayListActionStatus
    data class NoChange(val present: Boolean) : RelayListActionStatus
    data class Receipt(val id: ULong, val status: WriteStatus) : RelayListActionStatus
    data class Failed(val failure: RelayListActionFailure) : RelayListActionStatus

    companion object {
        fun from(ffi: FfiRelayListActionStatus): RelayListActionStatus =
            when (ffi) {
                is FfiRelayListActionStatus.Acquiring -> Acquiring
                is FfiRelayListActionStatus.NoChange -> NoChange(ffi.present)
                is FfiRelayListActionStatus.Receipt ->
                    Receipt(ffi.receiptId, WriteStatus.from(ffi.status))
                is FfiRelayListActionStatus.Failed ->
                    Failed(RelayListActionFailure.from(ffi.failure))
            }
    }
}

data class RelayListAction(val status: Flow<RelayListActionStatus>)

internal fun relayListAction(
    engine: NmpEngineInterface,
    relay: String,
    adding: Boolean,
): RelayListAction {
    val channel = Channel<RelayListActionStatus>(Channel.UNLIMITED)
    val observer =
        object : RelayListActionObserver {
            override fun onStatus(status: FfiRelayListActionStatus) {
                channel.trySendBlocking(RelayListActionStatus.from(status))
            }

            override fun onClosed() {
                channel.close()
            }
        }
    if (adding) {
        engine.addSimpleGroupRelay(relay, observer)
    } else {
        engine.removeSimpleGroupRelay(relay, observer)
    }
    return RelayListAction(channel.receiveAsFlow())
}
