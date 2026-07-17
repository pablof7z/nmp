// Typed NIP-22 comments over NIP-73 external targets (#572) -- pure
// functions, same shape as NIP29.kt's precedent (#108): no `NMPEngine`
// instance is needed for root-thread demand or decode. `NMPEngine.
// commentIntent` (this file's write-side counterpart) needs no engine
// state either -- `nmp_nip22::comment_intent` takes author/time as
// explicit caller parameters -- but lives on `NMPEngine` for the same
// "engine door" naming symmetry as `groupMessageIntent`. Mirrors
// NIP22.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiCommentDecodeException
import uniffi.nmp_ffi.FfiCommentParent
import uniffi.nmp_ffi.FfiCommentRoot
import uniffi.nmp_ffi.FfiComposedWriteIntent
import uniffi.nmp_ffi.FfiDecodedComment
import uniffi.nmp_ffi.FfiNip73Target
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.NmpEngineInterface
import uniffi.nmp_ffi.commentThreadDemand as ffiCommentThreadDemand
import uniffi.nmp_ffi.decodeComment as ffiDecodeComment

/** A validated NIP-73 external-content target (`FfiNip73Target` mirror). */
sealed class Nip73Target {
    data class PodcastEpisodeGuid(val guid: String) : Nip73Target()

    data class General(val value: String, val kind: String) : Nip73Target()

    internal fun toFfi(): FfiNip73Target =
        when (this) {
            is PodcastEpisodeGuid -> FfiNip73Target.PodcastEpisodeGuid(guid)
            is General -> FfiNip73Target.General(value, kind)
        }

    companion object {
        internal fun from(ffi: FfiNip73Target): Nip73Target =
            when (ffi) {
                is FfiNip73Target.PodcastEpisodeGuid -> PodcastEpisodeGuid(ffi.guid)
                is FfiNip73Target.General -> General(ffi.value, ffi.kind)
            }
    }
}

/** The root of a NIP-22 comment thread (`FfiCommentRoot` mirror). Every
 * comment in a thread, regardless of nesting depth, carries an IDENTICAL
 * root value. */
sealed class CommentRoot {
    data class Event(val eventId: String, val kind: UShort, val authorPubkey: String?) : CommentRoot()

    data class Address(val authorPubkey: String, val kind: UShort, val identifier: String) : CommentRoot()

    data class External(val target: Nip73Target) : CommentRoot()

    internal fun toFfi(): FfiCommentRoot =
        when (this) {
            is Event -> FfiCommentRoot.Event(eventId, kind, authorPubkey)
            is Address -> FfiCommentRoot.Address(authorPubkey, kind, identifier)
            is External -> FfiCommentRoot.External(target.toFfi())
        }

    companion object {
        internal fun from(ffi: FfiCommentRoot): CommentRoot =
            when (ffi) {
                is FfiCommentRoot.Event -> Event(ffi.eventId, ffi.kind, ffi.authorPubkey)
                is FfiCommentRoot.Address -> Address(ffi.authorPubkey, ffi.kind, ffi.identifier)
                is FfiCommentRoot.External -> External(Nip73Target.from(ffi.target))
            }
    }
}

/** A comment's direct parent (`FfiCommentParent` mirror). [Root] means this
 * is a TOP-LEVEL comment (its parent mirrors the root); [Comment] means it
 * replies to another comment event. */
sealed class CommentParent {
    data object Root : CommentParent()

    data class Comment(val eventId: String, val authorPubkey: String?) : CommentParent()

    internal fun toFfi(): FfiCommentParent =
        when (this) {
            is Root -> FfiCommentParent.Root
            is Comment -> FfiCommentParent.Comment(eventId, authorPubkey)
        }

    companion object {
        internal fun from(ffi: FfiCommentParent): CommentParent =
            when (ffi) {
                is FfiCommentParent.Root -> Root
                is FfiCommentParent.Comment -> Comment(ffi.eventId, ffi.authorPubkey)
            }
    }
}

/** A successfully decoded, typed NIP-22 comment (`FfiDecodedComment`
 * mirror). */
data class DecodedComment(
    val eventId: String,
    val authorPubkey: String,
    val createdAt: ULong,
    val content: String,
    val root: CommentRoot,
    val parent: CommentParent,
) {
    companion object {
        internal fun from(ffi: FfiDecodedComment): DecodedComment =
            DecodedComment(
                ffi.eventId,
                ffi.authorPubkey,
                ffi.createdAt,
                ffi.content,
                CommentRoot.from(ffi.root),
                CommentParent.from(ffi.parent),
            )
    }
}

/** `decodeComment`'s typed rejection (`FfiCommentDecodeException` mirror).
 * Exhaustive: malformed or mismatched tag sets stay raw rows, they never
 * become a typed comment. */
sealed class CommentDecodeError(message: String) : Exception(message) {
    data class WrongKind(val got: UShort) : CommentDecodeError("expected kind 1111, got $got")

    data object MissingRoot : CommentDecodeError("no root (E/A/I) tag present")

    data object DuplicateContradictoryRoot :
        CommentDecodeError("more than one distinct root (E/A/I) tag present")

    data object MissingRootKind : CommentDecodeError("root tag present without its required K")

    data class InvalidRootKind(val got: String) :
        CommentDecodeError("root K $got is not a valid kind number")

    data object MalformedRootReference : CommentDecodeError("root E/A reference did not parse")

    data object EmptyExternalValue : CommentDecodeError("I/i or K/k cell was empty")

    data object MissingParent : CommentDecodeError("no parent (e/a/i) tag present")

    data object DuplicateContradictoryParent :
        CommentDecodeError("more than one distinct parent (e/a/i) tag present")

    data object MissingParentKind : CommentDecodeError("parent tag present without its required k")

    data class InvalidParentKind(val got: String) :
        CommentDecodeError("parent k $got is not a valid kind number")

    data object MalformedParentReference : CommentDecodeError("parent e/a reference did not parse")

    data object ParentDoesNotMatchRootOrComment : CommentDecodeError(
        "parent tag neither mirrors the root nor is a valid e+k=1111 comment reference",
    )

    companion object {
        internal fun from(ffi: FfiCommentDecodeException): CommentDecodeError =
            when (ffi) {
                is FfiCommentDecodeException.WrongKind -> WrongKind(ffi.got)
                is FfiCommentDecodeException.MissingRoot -> MissingRoot
                is FfiCommentDecodeException.DuplicateContradictoryRoot -> DuplicateContradictoryRoot
                is FfiCommentDecodeException.MissingRootKind -> MissingRootKind
                is FfiCommentDecodeException.InvalidRootKind -> InvalidRootKind(ffi.got)
                is FfiCommentDecodeException.MalformedRootReference -> MalformedRootReference
                is FfiCommentDecodeException.EmptyExternalValue -> EmptyExternalValue
                is FfiCommentDecodeException.MissingParent -> MissingParent
                is FfiCommentDecodeException.DuplicateContradictoryParent -> DuplicateContradictoryParent
                is FfiCommentDecodeException.MissingParentKind -> MissingParentKind
                is FfiCommentDecodeException.InvalidParentKind -> InvalidParentKind(ffi.got)
                is FfiCommentDecodeException.MalformedParentReference -> MalformedParentReference
                is FfiCommentDecodeException.ParentDoesNotMatchRootOrComment ->
                    ParentDoesNotMatchRootOrComment
            }
    }
}

/** The demand for an entire NIP-22 comment thread rooted at [root]:
 * `kinds:[1111]`, scoped by the uppercase root reference on `#I`. One
 * filter covers the whole thread -- top-level comments AND every reply.
 * Throws `NMPError` if [root] fails to parse (e.g. a malformed pubkey/
 * event id hex, or an empty NIP-73 target cell). */
fun commentThreadDemand(root: CommentRoot): NMPDemand =
    NMPDemand.from(nmpRethrowing { ffiCommentThreadDemand(root.toFfi()) })

/** Decode a delivered kind:1111 [Row] into a typed [DecodedComment].
 * Fallible: malformed or mismatched tag sets throw [CommentDecodeError]
 * and never become a typed comment. */
fun decodeComment(row: Row): DecodedComment {
    val ffiRow =
        FfiRow(
            id = row.id,
            pubkey = row.pubkey,
            createdAt = row.createdAt,
            kind = row.kind,
            tags = row.tags,
            content = row.content,
            sig = row.sig,
            sources = row.sources,
        )
    try {
        return DecodedComment.from(ffiDecodeComment(ffiRow))
    } catch (error: FfiCommentDecodeException) {
        throw CommentDecodeError.from(error)
    }
}

/** A composed NIP-22 comment (#572), returned by [NMPEngine.commentIntent].
 * Opaque and take-once -- pass it to `NMPEngine.publishComposed` exactly
 * once; a second attempt throws `NMPError.IntentAlreadyConsumed`. Never
 * exposes the materialized tags, routing, author, or timestamp. */
class CommentIntent internal constructor(internal val ffi: FfiComposedWriteIntent)

/** Compose a durable, author-outbox-routed NIP-22 comment `WriteIntent`
 * (#572). Unlike `groupMessageIntent`, this needs no engine state at all --
 * author/time are explicit caller parameters -- but lives here for the same
 * "engine door" naming symmetry. [correlation] (#591) passes straight
 * through to `WriteIntent.correlation`. Publish the returned take-once
 * value through `NMPEngine.publishComposed`. */
internal fun composeCommentIntent(
    engine: NmpEngineInterface,
    root: CommentRoot,
    parent: CommentParent,
    authorPubkey: String,
    createdAt: ULong,
    content: String,
    correlation: String?,
): CommentIntent =
    CommentIntent(
        nmpRethrowing {
            engine.commentIntent(
                root.toFfi(),
                parent.toFfi(),
                authorPubkey,
                createdAt,
                content,
                correlation,
            )
        },
    )
