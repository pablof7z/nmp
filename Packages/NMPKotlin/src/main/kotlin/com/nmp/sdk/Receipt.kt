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

/** Publish a [GroupSendIntent] from `groupSendIntent` (#115). Take-once:
 * `intent` is consumed by this call -- a second `publishComposed` on the
 * SAME [GroupSendIntent] throws `NMPError.IntentAlreadyConsumed` rather
 * than silently re-publishing a stale template (recompose via
 * `groupSendIntent` again for a retry). Otherwise identical to
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
