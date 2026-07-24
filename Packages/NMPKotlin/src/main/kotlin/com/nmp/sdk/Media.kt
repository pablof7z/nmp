// Witness-typed NIP-68 picture composition (#730, epic #216).
//
// VerifiedUpload -> PictureImage -> PictureDraft
//
// A listed descriptor or raw imeta row cannot enter this path. The immutable
// draft feeds the existing sign-only and write-intent APIs; upload,
// composition, signing, and publication remain separate failure domains.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiComposedImage
import uniffi.nmp_ffi.FfiImageDim
import uniffi.nmp_ffi.FfiPictureComposeException
import uniffi.nmp_ffi.FfiPictureContentWarning
import uniffi.nmp_ffi.FfiPictureDraft
import uniffi.nmp_ffi.FfiPicturePost
import uniffi.nmp_ffi.composePicture as ffiComposePicture

/** Pixel dimensions for one NIP-68 image. */
data class ImageDimensions(
    val width: UInt,
    val height: UInt,
) {
    internal fun toFfi(): FfiImageDim = FfiImageDim(width, height)
}

/** One verified upload plus app-owned presentation metadata. Mandatory
 * `url`/`m`/`x` provenance comes only from [upload]. */
class PictureImage(
    internal val upload: VerifiedUpload,
    val dimensions: ImageDimensions? = null,
    val alt: String? = null,
    val blurhash: String? = null,
    val thumbhash: String? = null,
    val fallbacks: List<String> = emptyList(),
) {
    internal fun toFfi(): FfiComposedImage =
        FfiComposedImage(
            upload = upload.ffi,
            dim = dimensions?.toFfi(),
            alt = alt,
            blurhash = blurhash,
            thumbhash = thumbhash,
            fallbacks = fallbacks,
        )
}

/** Optional NIP-68 `content-warning` tag. A value with [reason] `null`
 * represents the one-cell tag; a null [PicturePost.contentWarning] omits it. */
data class PictureContentWarning(
    val reason: String? = null,
) {
    internal fun toFfi(): FfiPictureContentWarning = FfiPictureContentWarning(reason)
}

/** Event-level metadata for a kind:20 picture post. */
data class PicturePost(
    val title: String? = null,
    val description: String,
    val contentWarning: PictureContentWarning? = null,
    val hashtags: List<String> = emptyList(),
) {
    internal fun toFfi(): FfiPicturePost =
        FfiPicturePost(title, description, contentWarning?.toFfi(), hashtags)
}

/** Composition-only failures. Upload, signer, and receipt failures retain
 * their existing independent types. */
sealed class PictureComposeError(message: String) : Exception(message) {
    data class InvalidAuthorPubkey(val got: String) :
        PictureComposeError("invalid picture author public key: $got")

    object NoImages :
        PictureComposeError("cannot compose a kind:20 picture without verified images")

    object ImageMissingMimeType :
        PictureComposeError("verified image descriptor has no NIP-68 mime type")

    object EmptyHashtag :
        PictureComposeError("cannot compose a kind:20 picture with an empty hashtag")

    companion object {
        internal fun from(ffi: FfiPictureComposeException): PictureComposeError =
            when (ffi) {
                is FfiPictureComposeException.InvalidAuthorPubkey -> InvalidAuthorPubkey(ffi.got)
                is FfiPictureComposeException.NoImages -> NoImages
                is FfiPictureComposeException.ImageMissingMimeType -> ImageMissingMimeType
                is FfiPictureComposeException.EmptyHashtag -> EmptyHashtag
            }
    }
}

/** Immutable kind:20 draft produced only by [composePicture]. Deliberately
 * not a data class: a generated public `copy()` would be a constructor-shaped
 * escape hatch for replacing kind/tags/content. */
class PictureDraft internal constructor(
    val authorPubkeyHex: String,
    val createdAt: ULong,
    val kind: UShort,
    val tags: List<List<String>>,
    val content: String,
    val unsignedEventJson: String,
) {
    internal constructor(ffi: FfiPictureDraft) : this(
        ffi.authorPubkeyHex(),
        ffi.createdAt(),
        ffi.kind(),
        ffi.tags(),
        ffi.content(),
        ffi.unsignedEventJson(),
    )

    /** Existing governed sign-only body. Its author comes from the engine's
     * active identity, which must equal [authorPubkeyHex]; ordinary
     * publication should use [writeIntent]. */
    val signRequest: NMPUnsignedEvent
        get() = NMPUnsignedEvent(createdAt, kind, tags, content)

    /** Existing ordinary write intent for this exact picture body. */
    fun writeIntent(
        durability: Durability,
        routing: WriteRouting,
        identityOverride: String? = null,
        correlation: String? = null,
    ): WriteIntent =
        WriteIntent(
            payload = WritePayload.Unsigned(authorPubkeyHex, createdAt, kind, tags, content),
            durability = durability,
            routing = routing,
            identityOverride = identityOverride,
            correlation = correlation,
        )
}

/** Compose a kind:20 draft from verified uploads. There is no raw descriptor,
 * raw `imeta`, event-kind, or numeric routing parameter. */
fun composePicture(
    authorPubkeyHex: String,
    createdAt: ULong,
    images: List<PictureImage>,
    post: PicturePost,
): PictureDraft =
    try {
        PictureDraft(
            ffiComposePicture(
                authorPubkeyHex,
                createdAt,
                images.map { it.toFfi() },
                post.toFfi(),
            ),
        )
    } catch (error: FfiPictureComposeException) {
        throw PictureComposeError.from(error)
    }
