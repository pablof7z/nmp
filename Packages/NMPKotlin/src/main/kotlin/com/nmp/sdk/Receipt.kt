package com.nmp.sdk

import kotlinx.coroutines.channels.Channel
import kotlinx.coroutines.channels.trySendBlocking
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.receiveAsFlow
import uniffi.nmp_ffi.FfiReceiptReattachment
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

private class ReceiptBridge {
    val channel = Channel<WriteStatus>(Channel.UNLIMITED)
    val observer =
        object : ReceiptObserver {
            override fun onStatus(status: FfiWriteStatus) {
                channel.trySendBlocking(WriteStatus.from(status))
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
