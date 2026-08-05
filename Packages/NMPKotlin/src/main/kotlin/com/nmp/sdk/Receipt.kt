package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiReceiptReattachment
import uniffi.nmp_ffi.FfiCancelWriteException
import uniffi.nmp_ffi.FfiRemoveQueueEntryException
import uniffi.nmp_ffi.FfiCancelWriteOutcome
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.NmpReceiptStream

/** A stable receipt identity and its stream of retained/live write facts. */
data class Receipt(
    val id: ULong,
    val status: Flow<WriteFact>,
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

    data class AlreadySuperseded(val receiptId: ULong) :
        NMPWriteCancellationError("receipt $receiptId was superseded by a newer write")

    /** The write was refused at acceptance and is already a permanently
     * failed queue entry. There is nothing to cancel; remove it instead. */
    data class AlreadyRefused(val receiptId: ULong) :
        NMPWriteCancellationError("receipt $receiptId was refused at acceptance")

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
                is FfiCancelWriteException.AlreadySuperseded ->
                    AlreadySuperseded(error.receiptId)
                is FfiCancelWriteException.AlreadyRefused ->
                    AlreadyRefused(error.receiptId)
                is FfiCancelWriteException.PersistenceFailed ->
                    PersistenceFailed(error.receiptId, error.reason)
                is FfiCancelWriteException.EngineClosed -> EngineClosed
            }
    }
}

enum class WriteCancellationOutcome {
    Cancelled,
}

/** Typed refusals from the queue-entry removal door. */
sealed class NMPQueueEntryRemovalError(message: String) : Exception(message) {
    data class UnknownReceipt(val receiptId: ULong) :
        NMPQueueEntryRemovalError("unknown receipt $receiptId")

    /** The write still owns open delivery work. Cancel it first; removal is
     * for entries nothing is going to move. */
    data class StillActive(val receiptId: ULong) :
        NMPQueueEntryRemovalError("receipt $receiptId still owns open delivery work; cancel it first")

    data class PersistenceFailed(val receiptId: ULong, val reason: String) :
        NMPQueueEntryRemovalError("could not remove queue entry for receipt $receiptId: $reason")

    object EngineClosed : NMPQueueEntryRemovalError("engine already shut down")

    companion object {
        internal fun from(error: FfiRemoveQueueEntryException): NMPQueueEntryRemovalError =
            when (error) {
                is FfiRemoveQueueEntryException.UnknownReceipt -> UnknownReceipt(error.receiptId)
                is FfiRemoveQueueEntryException.StillActive -> StillActive(error.receiptId)
                is FfiRemoveQueueEntryException.PersistenceFailed ->
                    PersistenceFailed(error.receiptId, error.reason)
                is FfiRemoveQueueEntryException.EngineClosed -> EngineClosed
            }
    }
}

/** Read the app's own publish queue back.
 *
 * Answers "what have I got outstanding, and what went wrong with it" without
 * having held a receipt stream open since acceptance. This is INSPECTION: it
 * never blocks and never waits for settlement. */
internal fun publishQueue(engine: NmpEngineInterface): List<PublishQueueEntry> =
    try {
        engine.publishQueue().map { PublishQueueEntry.from(it) }
    } catch (error: FfiRemoveQueueEntryException) {
        throw NMPQueueEntryRemovalError.from(error)
    }

/** Forget one queue entry.
 *
 * A real TERMINATION path, not housekeeping: a write parked forever on a
 * signer that never attached, and a permanently-failed refused entry, end no
 * other way. A write that still owns open delivery work is refused -- cancel
 * that one instead. */
internal fun removePublishQueueEntry(engine: NmpEngineInterface, receiptId: ULong) =
    try {
        engine.removePublishQueueEntry(receiptId)
    } catch (error: FfiRemoveQueueEntryException) {
        throw NMPQueueEntryRemovalError.from(error)
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
 * The live FIFO is finite and reports [NMPError.FactStreamLagged] rather than
 * growing or dropping silently; reattachment transparently traverses durable
 * facts in finite pages. Collection-scope teardown withdraws the LIVE stream
 * via `handle.cancel()`; the durable receipt itself is untouched. */
internal fun receiptFrom(stream: NmpReceiptStream): Receipt =
    Receipt(id = stream.id(), status = receiptStatusFlow(stream))

private fun receiptStatusFlow(stream: NmpReceiptStream): Flow<WriteFact> =
    flow {
        try {
            while (true) {
                val status = nmpRethrowingAsync { stream.next() } ?: break
                emit(WriteFact.from(status))
            }
        } finally {
            stream.cancel()
        }
    }

/** Enqueue immediately and retain the store-issued id needed for reattach. */
fun publishReceipt(engine: NmpEngineInterface, intent: WriteIntent): Receipt =
    receiptFrom(nmpRethrowing { engine.publish(intent.toFfi()) })

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

/** #591: recover a receipt after a crash that happened BEFORE the app could
 * durably persist the receipt id `publish` returned --
 * looked up by the caller's own crash-safe correlation token instead.
 * Otherwise identical to [reattachReceipt] (the by-id overload). The resolved
 * receipt id (#591) rides along on the attached stream handle itself
 * ([Receipt.id]); a token-only caller learns it there. */
fun reattachReceiptByCorrelation(engine: NmpEngineInterface, correlation: String): ReceiptReattachment =
    mapReceiptReattachment(
        nmpRethrowing { engine.reattachByCorrelation(correlation).outcome },
        ::receiptFrom,
    )
