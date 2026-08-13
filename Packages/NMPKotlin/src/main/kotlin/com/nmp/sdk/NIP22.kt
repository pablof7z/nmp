// Typed NIP-22 comments over NIP-73 external content ids (#572/#822/#1258). Demand,
// decode, and composition are pure protocol-owned functions. Composition
// returns NMP's ordinary WriteIntent; publication remains exclusively on
// NMPEngine.publish. Mirrors NIP22.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiCommentDecodeException
import uniffi.nmp_ffi.FfiCommentParent
import uniffi.nmp_ffi.FfiCommentRoot
import uniffi.nmp_ffi.FfiCommentTarget
import uniffi.nmp_ffi.FfiDecodedComment
import uniffi.nmp_ffi.FfiNip73
import uniffi.nmp_ffi.FfiRow
import uniffi.nmp_ffi.commentIntent as ffiCommentIntent
import uniffi.nmp_ffi.commentThreadDemand as ffiCommentThreadDemand
import uniffi.nmp_ffi.decodeComment as ffiDecodeComment

/** A validated NIP-73 external content id (`FfiNip73` mirror).
 *
 * [Url] states the page a caller means; Rust normalises it (NIP-73's table:
 * "URL, normalized, no fragment"), so a value read back from a decoded
 * comment carries the canonical spelling rather than the one that was sent.
 * Normalising here as well would be a second owner of one rule. */
sealed class Nip73 {
    data class PodcastEpisode(val guid: String) : Nip73()

    data class Url(val url: String) : Nip73()

    data class General(val value: String, val kind: String) : Nip73()

    internal fun toFfi(): FfiNip73 =
        when (this) {
            is PodcastEpisode -> FfiNip73.PodcastEpisode(guid)
            is Url -> FfiNip73.Url(url)
            is General -> FfiNip73.General(value, kind)
        }

    companion object {
        internal fun from(ffi: FfiNip73): Nip73 =
            when (ffi) {
                is FfiNip73.PodcastEpisode -> PodcastEpisode(ffi.guid)
                is FfiNip73.Url -> Url(ffi.url)
                is FfiNip73.General -> General(ffi.value, ffi.kind)
            }
    }
}

/** The root of a NIP-22 comment thread (`FfiCommentRoot` mirror). Every
 * comment in a thread, regardless of nesting depth, carries an IDENTICAL
 * root value. */
sealed class CommentRoot {
    data class Event(val eventId: String, val kind: UShort, val authorPubkey: String?) : CommentRoot()

    /** [eventId]: the addressable event's own id, when pinned alongside the
     * coordinate (NIP-22: "when the parent event is replaceable or
     * addressable, also include an `e`/`E` tag referencing its id"). `null`
     * remains a fully legal root. */
    data class Address(
        val authorPubkey: String,
        val kind: UShort,
        val identifier: String,
        val eventId: String? = null,
    ) : CommentRoot()

    data class External(val target: Nip73) : CommentRoot()

    internal fun toFfi(): FfiCommentRoot =
        when (this) {
            is Event -> FfiCommentRoot.Event(eventId, kind, authorPubkey)
            is Address -> FfiCommentRoot.Address(authorPubkey, kind, identifier, eventId)
            is External -> FfiCommentRoot.External(target.toFfi())
        }

    companion object {
        internal fun from(ffi: FfiCommentRoot): CommentRoot =
            when (ffi) {
                is FfiCommentRoot.Event -> Event(ffi.eventId, ffi.kind, ffi.authorPubkey)
                is FfiCommentRoot.Address ->
                    Address(ffi.authorPubkey, ffi.kind, ffi.identifier, ffi.eventId)
                is FfiCommentRoot.External -> External(Nip73.from(ffi.target))
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

    /** A `K`/`k` cell of `podcast:item:guid` declared an `I`/`i` value that
     * did NOT carry the required `podcast:item:guid:` prefix. */
    data class MalformedExternalValue(val got: String) :
        CommentDecodeError("I/i value $got does not carry the prefix its K/k cell requires")

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

    /** The delivered [Row]'s OWN `id`/`pubkey` envelope fields were not
     * valid hex -- distinct from [MalformedRootReference], which describes
     * a root `E`/`A` TAG reference, never the row's own envelope. */
    data class MalformedRowEnvelope(val reason: String) :
        CommentDecodeError("row envelope is malformed: $reason")

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
                is FfiCommentDecodeException.MalformedExternalValue -> MalformedExternalValue(ffi.got)
                is FfiCommentDecodeException.MissingParent -> MissingParent
                is FfiCommentDecodeException.DuplicateContradictoryParent -> DuplicateContradictoryParent
                is FfiCommentDecodeException.MissingParentKind -> MissingParentKind
                is FfiCommentDecodeException.InvalidParentKind -> InvalidParentKind(ffi.got)
                is FfiCommentDecodeException.MalformedParentReference -> MalformedParentReference
                is FfiCommentDecodeException.ParentDoesNotMatchRootOrComment ->
                    ParentDoesNotMatchRootOrComment
                is FfiCommentDecodeException.MalformedRowEnvelope ->
                    MalformedRowEnvelope(ffi.reason)
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
            signature = row.signature.toFfi(),
            sources = row.sources,
        )
    try {
        return DecodedComment.from(ffiDecodeComment(ffiRow))
    } catch (error: FfiCommentDecodeException) {
        throw CommentDecodeError.from(error)
    }
}

/** What a comment is being written on (`FfiCommentTarget` mirror).
 *
 * The two shapes are the two things an app actually holds. [Root] describes
 * an entity by its parts -- what an app has for an external content id, or
 * after decoding a comment. [Row] is an event NMP observed, and its own
 * thread position is read off its own rows: replying to a deep comment and
 * commenting on a root are then the same call, and the root cannot be
 * restated wrongly by a caller who thought it knew. */
sealed class CommentTarget {
    data class Root(val root: CommentRoot) : CommentTarget()

    data class Row(val row: com.nmp.sdk.Row) : CommentTarget()

    internal fun toFfi(): FfiCommentTarget =
        when (this) {
            is Root -> FfiCommentTarget.Root(root.toFfi())
            is Row -> FfiCommentTarget.Row(row.toFfi())
        }
}

/** Compose a NIP-22 comment on [target] as NMP's ordinary [WriteIntent]
 * (#822). It names no author and reads no clock -- the engine resolves the
 * identity and stamps the time at acceptance -- so composition still owns no
 * engine state or lifecycle. [correlation] passes through unchanged; publish
 * the result through [NMPEngine.publish].
 *
 * This always composes a kind:1111 comment, including on a text note, where
 * [replyTo] would compose a NIP-10 reply instead. An app that wants "the
 * ordinary reply for whatever this is" calls that; an app that wants "a
 * NIP-22 comment on this specifically" calls this. */
fun commentIntent(
    target: CommentTarget,
    content: String,
    correlation: String? = null,
): WriteIntent =
    WriteIntent.from(
        nmpRethrowing {
            ffiCommentIntent(
                target.toFfi(),
                content,
                correlation,
            )
        },
    )
