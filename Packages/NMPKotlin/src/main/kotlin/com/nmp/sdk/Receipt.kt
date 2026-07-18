package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiCancelWriteException
import uniffi.nmp_ffi.FfiCancelWriteOutcome
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpReceiptStream

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

/** Build the ergonomic [Receipt] from a live [NmpReceiptStream] (#680). The
 * stable store-issued id is read synchronously off the handle; the status
 * `Flow` is a cold pull loop over `next()` -- FIFO write facts, no folding
 * and no conflation (receipts are durable facts, not disposable snapshots).
 * Collection-scope teardown withdraws the LIVE stream via `handle.cancel()`;
 * the durable receipt itself is untouched (a later `reattachReceipt` replays
 * the durable prefix from the store). */
private fun receiptFrom(stream: NmpReceiptStream): Receipt =
    Receipt(id = stream.id(), status = receiptStatusFlow(stream))

private fun receiptStatusFlow(stream: NmpReceiptStream): Flow<WriteStatus> =
    flow {
        try {
            while (true) {
                val status = nmpRethrowingAsync { stream.next() } ?: break
                emit(WriteStatus.from(status))
            }
        } finally {
            stream.cancel()
        }
    }

/** Enqueue immediately and retain the store-issued id needed for reattach. */
fun publishReceipt(engine: NmpEngineInterface, intent: WriteIntent): Receipt =
    receiptFrom(nmpRethrowing { engine.publish(intent.toFfi()) })

/** Publish a [GroupSendIntent] from `groupMessageIntent` (#156). Take-once:
 * `intent` is consumed by this call -- a second `publishComposed` on the
 * SAME [GroupSendIntent] throws `NMPError.IntentAlreadyConsumed` rather
 * than silently re-publishing a stale template (recompose via
 * `groupMessageIntent` again for a retry). Otherwise identical to
 * [publishReceipt]. */
fun publishComposedReceipt(engine: NmpEngineInterface, intent: GroupSendIntent): Receipt =
    receiptFrom(nmpRethrowing { engine.publishComposed(intent.ffi) })

/** Map the reattachment outcome without collapsing corrupt retained
 * evidence into the same result as an unknown id (#680). Extracted with an
 * injectable `attach` so the [ReceiptReattachment.NotFound] /
 * [ReceiptReattachment.RetainedButUnreadable] distinction is unit-testable
 * without a live [NmpReceiptStream]. */
internal fun mapReceiptReattachment(
    result: FfiReceiptReattachment,
    attach: (NmpReceiptStream) -> Receipt,
): ReceiptReattachment =
    when (result) {
        is FfiReceiptReattachment.Attached -> ReceiptReattachment.Attached(attach(result.stream))
        FfiReceiptReattachment.NotFound -> ReceiptReattachment.NotFound
        FfiReceiptReattachment.RetainedButUnreadable ->
            ReceiptReattachment.RetainedButUnreadable
    }

/** Attach without collapsing corrupt retained evidence into absence. */
fun reattachReceipt(engine: NmpEngineInterface, id: ULong): ReceiptReattachment =
    mapReceiptReattachment(nmpRethrowing { engine.reattachReceipt(id) }, ::receiptFrom)
