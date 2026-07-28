package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiReceiptIdentity
import uniffi.nmp_ffi.FfiReceiptSetEvent
import uniffi.nmp_ffi.FfiReceiptSetException

sealed interface ReceiptSetIdentity {
    data class Id(val receiptId: ULong) : ReceiptSetIdentity
    data class Correlation(val token: String) : ReceiptSetIdentity

    fun toFfi(): FfiReceiptIdentity =
        when (this) {
            is Id -> FfiReceiptIdentity.Id(receiptId)
            is Correlation -> FfiReceiptIdentity.Correlation(token)
        }
}

sealed interface ReceiptSetEvent {
    data class Fact(
        val identity: ReceiptSetIdentity,
        val receiptId: ULong,
        val status: WriteStatus,
    ) : ReceiptSetEvent

    data class NotFound(val identity: ReceiptSetIdentity) : ReceiptSetEvent
    data class RetainedButUnreadable(
        val identity: ReceiptSetIdentity,
        val receiptId: ULong?,
    ) : ReceiptSetEvent
    data class ReplayAfterLag(
        val identity: ReceiptSetIdentity,
        val receiptId: ULong,
    ) : ReceiptSetEvent
    data class ReplayUnavailable(
        val identity: ReceiptSetIdentity,
        val receiptId: ULong,
    ) : ReceiptSetEvent
    data class Closed(
        val identity: ReceiptSetIdentity,
        val receiptId: ULong,
    ) : ReceiptSetEvent
}

sealed class NMPReceiptSetError(message: String) : Exception(message) {
    data class CapacityExceeded(val capacity: ULong, val requested: ULong) :
        NMPReceiptSetError("receipt set capacity $capacity exceeded by $requested identities")
    data class DuplicateIdentity(val identity: String) :
        NMPReceiptSetError("duplicate receipt identity $identity")
    data object EngineClosed : NMPReceiptSetError("engine already shut down")
}

val NMPEngine.receiptSetCapacity: ULong
    get() = ffi.receiptSetCapacity()

fun NMPEngine.observeReceipts(identities: List<ReceiptSetIdentity>): Flow<ReceiptSetEvent> {
    val stream =
        try {
            ffi.observeReceipts(identities.map(ReceiptSetIdentity::toFfi))
        } catch (error: FfiReceiptSetException) {
            throw when (error) {
                is FfiReceiptSetException.CapacityExceeded ->
                    NMPReceiptSetError.CapacityExceeded(error.capacity, error.requested)
                is FfiReceiptSetException.DuplicateIdentity ->
                    NMPReceiptSetError.DuplicateIdentity(error.identity)
                is FfiReceiptSetException.EngineClosed -> NMPReceiptSetError.EngineClosed
            }
        }
    return flow {
        try {
            while (true) {
                val event = nmpRethrowingAsync { stream.next() } ?: break
                emit(event.toSdk())
            }
        } finally {
            stream.cancel()
        }
    }
}

private fun FfiReceiptIdentity.toSdk(): ReceiptSetIdentity =
    when (this) {
        is FfiReceiptIdentity.Id -> ReceiptSetIdentity.Id(receiptId)
        is FfiReceiptIdentity.Correlation -> ReceiptSetIdentity.Correlation(token)
    }

private fun FfiReceiptSetEvent.toSdk(): ReceiptSetEvent =
    when (this) {
        is FfiReceiptSetEvent.Fact ->
            ReceiptSetEvent.Fact(identity.toSdk(), receiptId, WriteStatus.from(status))
        is FfiReceiptSetEvent.NotFound -> ReceiptSetEvent.NotFound(identity.toSdk())
        is FfiReceiptSetEvent.RetainedButUnreadable ->
            ReceiptSetEvent.RetainedButUnreadable(identity.toSdk(), receiptId)
        is FfiReceiptSetEvent.ReplayAfterLag ->
            ReceiptSetEvent.ReplayAfterLag(identity.toSdk(), receiptId)
        is FfiReceiptSetEvent.ReplayUnavailable ->
            ReceiptSetEvent.ReplayUnavailable(identity.toSdk(), receiptId)
        is FfiReceiptSetEvent.Closed -> ReceiptSetEvent.Closed(identity.toSdk(), receiptId)
    }
