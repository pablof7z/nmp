// Opaque typed NIP-25 target/draft projection (#155). Target qualification
// re-reads the canonical Rust store by event id; caller-provided Row fields
// and sources are never trusted inputs. Mirrors NIP25.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiProtocolDraft
import uniffi.nmp_ffi.FfiReactionException
import uniffi.nmp_ffi.FfiReactionTarget
import uniffi.nmp_ffi.FfiReactionValue

/** One validated semantic NIP-25 reaction value. */
sealed class ReactionValue {
    data object Like : ReactionValue()

    data object Dislike : ReactionValue()

    data class Emoji(val value: String) : ReactionValue()

    data class CustomEmoji(val shortcode: String, val imageUrl: String) : ReactionValue()

    internal fun toFfi(): FfiReactionValue =
        when (this) {
            Like -> FfiReactionValue.Like
            Dislike -> FfiReactionValue.Dislike
            is Emoji -> FfiReactionValue.Emoji(value)
            is CustomEmoji -> FfiReactionValue.CustomEmoji(shortcode, imageUrl)
        }
}

/** Opaque capability proving that NMP qualified one canonical signed event
 * as a complete native-event reaction target. */
class ReactionTarget internal constructor(
    internal val ffi: FfiReactionTarget,
)

/** Opaque immutable unsigned protocol draft. No event kind, tags, author,
 * time, routing, signing, receipt, retry, or publication operation is
 * exposed. */
class ProtocolDraft internal constructor(
    internal val ffi: FfiProtocolDraft,
)

/** Typed failures from NIP-25 target qualification and draft composition. */
sealed class ReactionError(message: String) : Exception(message) {
    data class InvalidEventId(val got: String) : ReactionError("invalid Nostr event id: $got")

    data class TargetNotFound(val eventId: String) :
        ReactionError("event $eventId is not in the canonical NMP store")

    data class TargetNotVerified(val eventId: String) :
        ReactionError("canonical row $eventId is not a verified signed event")

    data class CanonicalLookupUnavailable(val reason: String) :
        ReactionError("canonical target lookup unavailable: $reason")

    data object EngineClosed : ReactionError("engine already shut down")

    data object NoActiveAccount : ReactionError("NIP-25 draft requires an active account")

    data object EmptyEmoji : ReactionError("Unicode reaction must not be empty")

    data class StandardValueRequiresTypedVariant(val got: String) :
        ReactionError("$got must use the typed like/dislike variant")

    data class CustomEmojiRequiresMetadata(val got: String) :
        ReactionError("$got requires matching typed NIP-30 metadata")

    data class InvalidEmojiToken(val got: String) :
        ReactionError("$got contains whitespace or control characters")

    data class InvalidCustomEmojiShortcode(val got: String) :
        ReactionError("invalid custom emoji shortcode: $got")

    data class InvalidCustomEmojiUrl(val got: String) :
        ReactionError("custom emoji image URL is not HTTP(S): $got")

    companion object {
        internal fun from(ffi: FfiReactionException): ReactionError =
            when (ffi) {
                is FfiReactionException.InvalidEventId -> InvalidEventId(ffi.got)
                is FfiReactionException.TargetNotFound -> TargetNotFound(ffi.eventId)
                is FfiReactionException.TargetNotVerified -> TargetNotVerified(ffi.eventId)
                is FfiReactionException.CanonicalLookupUnavailable ->
                    CanonicalLookupUnavailable(ffi.reason)
                is FfiReactionException.EngineClosed -> EngineClosed
                is FfiReactionException.NoActiveAccount -> NoActiveAccount
                is FfiReactionException.EmptyEmoji -> EmptyEmoji
                is FfiReactionException.StandardValueRequiresTypedVariant ->
                    StandardValueRequiresTypedVariant(ffi.got)
                is FfiReactionException.CustomEmojiRequiresMetadata ->
                    CustomEmojiRequiresMetadata(ffi.got)
                is FfiReactionException.InvalidEmojiToken -> InvalidEmojiToken(ffi.got)
                is FfiReactionException.InvalidCustomEmojiShortcode ->
                    InvalidCustomEmojiShortcode(ffi.got)
                is FfiReactionException.InvalidCustomEmojiUrl -> InvalidCustomEmojiUrl(ffi.got)
            }
    }
}

/** Qualify [row.id] through NMP's canonical cache. Every other field on this
 * caller-constructible row, including [Row.sources], is ignored. */
@Throws(ReactionError::class)
fun NMPEngine.reactionTarget(row: Row): ReactionTarget =
    try {
        ReactionTarget(ffi.reactionTarget(row.id))
    } catch (error: FfiReactionException) {
        throw ReactionError.from(error)
    }

/** Compose one Rust-authored unsigned NIP-25 draft using NMP's active
 * account and Rust-owned time. This does not publish the event. */
@Throws(ReactionError::class)
fun NMPEngine.reactionDraft(
    target: ReactionTarget,
    value: ReactionValue,
): ProtocolDraft =
    try {
        ProtocolDraft(ffi.reactionDraft(target.ffi, value.toFfi()))
    } catch (error: FfiReactionException) {
        throw ReactionError.from(error)
    }
