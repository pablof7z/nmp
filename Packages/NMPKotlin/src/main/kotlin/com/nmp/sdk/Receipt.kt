package com.nmp.sdk

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiCancelWriteException
import uniffi.nmp_ffi.FfiCancelWriteOutcome
import uniffi.nmp_ffi.FfiWriteStatus
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.ReceiptObserver

/** A stable receipt identity and its stream of retained/live write facts. */
data class Receipt(
    val id: ULong,
    val status: Flow<WriteStatus>,
)

sealed interface ReceiptReattachment {
    data class Attached(val receipt: Receipt) : ReceiptReattachment

    data object NotFound : ReceiptReattachment

    data object RetainedButUnreadable : ReceiptReattachment
}

sealed class NMPWriteCancellationError(message: String) : Exception(message) {
    data class UnknownReceipt(val receiptId: ULong) :
        NMPWriteCancellationError("unknown receipt $receiptId")

    data class AlreadySigned(val receiptId: ULong, val eventId: String) :
        NMPWriteCancellationError("receipt $receiptId is already signed as $eventId")

    data class AlreadyCompensated(val receiptId: ULong) :
        NMPWriteCancellationError("receipt $receiptId is already compensated")

    data class AlreadyAbandoned(val receiptId: ULong) :
        NMPWriteCancellationError("receipt $receiptId was abandoned after restart")

    data class PersistenceFailed(val receiptId: ULong, val reason: String) :
        NMPWriteCancellationError("could not persist cancellation for receipt $receiptId: $reason")

    object EngineClosed : NMPWriteCancellationError("engine already shut down")

    companion object {
        internal fun from(error: FfiCancelWriteException): NMPWriteCancellationError =
            when (error) {
                is FfiCancelWriteException.UnknownReceipt -> UnknownReceipt(error.receiptId)
                is FfiCancelWriteException.AlreadySigned ->
                    AlreadySigned(error.receiptId, error.eventId)
                is FfiCancelWriteException.AlreadyCompensated ->
                    AlreadyCompensated(error.receiptId)
                is FfiCancelWriteException.AlreadyAbandoned ->
                    AlreadyAbandoned(error.receiptId)
                is FfiCancelWriteException.PersistenceFailed ->
                    PersistenceFailed(error.receiptId, error.reason)
                is FfiCancelWriteException.EngineClosed -> EngineClosed
            }
    }
}

enum class WriteCancellationOutcome {
    Cancelled,
}

internal fun cancelWrite(engine: NmpEngineInterface, receiptId: ULong): WriteCancellationOutcome =
    try {
        when (engine.cancel(receiptId)) {
            FfiCancelWriteOutcome.CANCELLED -> WriteCancellationOutcome.Cancelled
        }
    } catch (error: FfiCancelWriteException) {
        throw NMPWriteCancellationError.from(error)
    }

private class ReceiptBridge {
    val channel = Channel<WriteStatus>(Channel.UNLIMITED)
    val observer =
        object : ReceiptObserver {
            override fun onStatus(status: FfiWriteStatus) {
                channel.trySendBlocking(WriteStatus.from(status))
            }

            override fun onClosed() {
                // The receipt `Sender` was dropped (intent resolved / engine
                // shut down) -- close the channel so its `Flow` completes,
                // mirroring Query.kt's `RowObserver` bridge.
                channel.close()
            }
        }

    fun receipt(id: ULong): Receipt = Receipt(id, channel.receiveAsFlow())
}

/** Enqueue immediately and retain the store-issued id needed for reattach. */
fun publishReceipt(engine: NmpEngineInterface, intent: WriteIntent): Receipt {
    val bridge = ReceiptBridge()
    val id = nmpRethrowing { engine.publish(intent.toFfi(), bridge.observer) }
    return bridge.receipt(id)
}

/** Publish a [GroupSendIntent] from `groupMessageIntent` (#156). Take-once:
 * `intent` is consumed by this call -- a second `publishComposed` on the
 * SAME [GroupSendIntent] throws `NMPError.IntentAlreadyConsumed` rather
 * than silently re-publishing a stale template (recompose via
 * `groupMessageIntent` again for a retry). Otherwise identical to
 * [publishReceipt]'s bridge. */
fun publishComposedReceipt(engine: NmpEngineInterface, intent: GroupSendIntent): Receipt {
    val bridge = ReceiptBridge()
    val id = nmpRethrowing { engine.publishComposed(intent.ffi, bridge.observer) }
    return bridge.receipt(id)
}

internal fun mapReceiptReattachment(
    result: FfiReceiptReattachment,
    id: ULong,
    status: Flow<WriteStatus>,
): ReceiptReattachment =
    when (result) {
        FfiReceiptReattachment.ATTACHED -> ReceiptReattachment.Attached(Receipt(id, status))
        FfiReceiptReattachment.NOT_FOUND -> ReceiptReattachment.NotFound
        FfiReceiptReattachment.RETAINED_BUT_UNREADABLE ->
            ReceiptReattachment.RetainedButUnreadable
    }

/** Attach without collapsing corrupt retained evidence into absence. */
fun reattachReceipt(engine: NmpEngineInterface, id: ULong): ReceiptReattachment {
    val bridge = ReceiptBridge()
    val result = nmpRethrowing { engine.reattachReceipt(id, bridge.observer) }
    if (result != FfiReceiptReattachment.ATTACHED) {
        bridge.channel.close()
    }
    return mapReceiptReattachment(result, id, bridge.channel.receiveAsFlow())
}
