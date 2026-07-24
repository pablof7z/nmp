// Witness-typed NIP-68 picture composition (#730, epic #216).
//
// The supported path is:
//
//   VerifiedUpload -> PictureImage -> PictureDraft
//
// `VerifiedUpload` has no public initializer, and `PictureImage` exposes no
// mandatory url/mime/sha256 fields. Consequently a listed descriptor or raw
// `imeta` row cannot enter this composer. The immutable draft then feeds the
// existing sign-only and write-intent APIs; upload, composition, signing, and
// publication remain separate failure domains.

import NMPFFI

/// Pixel dimensions for one NIP-68 image.
public struct ImageDimensions: Sendable, Hashable {
    public let width: UInt32
    public let height: UInt32

    public init(width: UInt32, height: UInt32) {
        self.width = width
        self.height = height
    }

    func toFfi() -> FfiImageDim {
        FfiImageDim(width: width, height: height)
    }
}

/// One verified upload plus app-owned presentation metadata. Mandatory
/// `url`/`m`/`x` provenance comes only from `upload`.
public struct PictureImage: Sendable {
    let upload: VerifiedUpload
    public let dimensions: ImageDimensions?
    public let alt: String?
    public let blurhash: String?
    public let thumbhash: String?
    public let fallbacks: [String]

    public init(
        upload: VerifiedUpload,
        dimensions: ImageDimensions? = nil,
        alt: String? = nil,
        blurhash: String? = nil,
        thumbhash: String? = nil,
        fallbacks: [String] = []
    ) {
        self.upload = upload
        self.dimensions = dimensions
        self.alt = alt
        self.blurhash = blurhash
        self.thumbhash = thumbhash
        self.fallbacks = fallbacks
    }

    func toFfi() -> FfiComposedImage {
        FfiComposedImage(
            upload: upload.ffi,
            dim: dimensions?.toFfi(),
            alt: alt,
            blurhash: blurhash,
            thumbhash: thumbhash,
            fallbacks: fallbacks
        )
    }
}

/// Optional NIP-68 `content-warning` tag. A value with `reason == nil`
/// represents the one-cell tag; `PicturePost.contentWarning == nil` omits it.
public struct PictureContentWarning: Sendable, Hashable {
    public let reason: String?

    public init(reason: String? = nil) {
        self.reason = reason
    }

    func toFfi() -> FfiPictureContentWarning {
        FfiPictureContentWarning(reason: reason)
    }
}

/// Event-level metadata for a kind:20 picture post.
public struct PicturePost: Sendable, Hashable {
    public let title: String?
    public let description: String
    public let contentWarning: PictureContentWarning?
    public let hashtags: [String]

    public init(
        title: String? = nil,
        description: String,
        contentWarning: PictureContentWarning? = nil,
        hashtags: [String] = []
    ) {
        self.title = title
        self.description = description
        self.contentWarning = contentWarning
        self.hashtags = hashtags
    }

    func toFfi() -> FfiPicturePost {
        FfiPicturePost(
            title: title,
            description: description,
            contentWarning: contentWarning?.toFfi(),
            hashtags: hashtags
        )
    }
}

/// Composition-only failures. Upload, signer, and receipt failures retain
/// their existing independent types.
public enum PictureComposeError: Error, Sendable, Hashable {
    case invalidAuthorPubkey(got: String)
    case noImages
    case imageMissingMimeType
    case emptyHashtag

    init(_ ffi: FfiPictureComposeError) {
        switch ffi {
        case .InvalidAuthorPubkey(let got):
            self = .invalidAuthorPubkey(got: got)
        case .NoImages:
            self = .noImages
        case .ImageMissingMimeType:
            self = .imageMissingMimeType
        case .EmptyHashtag:
            self = .emptyHashtag
        }
    }
}

/// Immutable kind:20 draft produced only by `composePicture`. Its event kind,
/// tags, and content cannot be replaced through this typed value.
public struct PictureDraft: Sendable, Hashable {
    public let authorPubkeyHex: String
    public let createdAt: UInt64
    public let kind: UInt16
    public let tags: [[String]]
    public let content: String
    public let unsignedEventJSON: String

    init(_ ffi: FfiPictureDraft) {
        authorPubkeyHex = ffi.authorPubkeyHex()
        createdAt = ffi.createdAt()
        kind = ffi.kind()
        tags = ffi.tags()
        content = ffi.content()
        unsignedEventJSON = ffi.unsignedEventJson()
    }

    /// Existing governed sign-only body. Its author comes from the engine's
    /// active identity, which must equal `authorPubkeyHex`; ordinary
    /// publication should use `writeIntent`.
    public var signRequest: NMPUnsignedEvent {
        NMPUnsignedEvent(createdAt: createdAt, kind: kind, tags: tags, content: content)
    }

    /// Existing ordinary write intent for this exact picture body. Routing
    /// and durability stay typed; event kind/tags/content are fixed here.
    public func writeIntent(
        durability: Durability,
        routing: WriteRouting,
        identityOverride: String? = nil,
        correlation: String? = nil
    ) -> WriteIntent {
        WriteIntent(
            payload: .unsigned(
                pubkey: authorPubkeyHex,
                createdAt: createdAt,
                kind: kind,
                tags: tags,
                content: content
            ),
            durability: durability,
            routing: routing,
            identityOverride: identityOverride,
            correlation: correlation
        )
    }
}

/// Compose a kind:20 draft from verified uploads. There is deliberately no
/// raw descriptor, raw `imeta`, event-kind, or numeric routing parameter.
public func composePicture(
    authorPubkeyHex: String,
    createdAt: UInt64,
    images: [PictureImage],
    post: PicturePost
) throws -> PictureDraft {
    do {
        return PictureDraft(
            try NMPFFI.composePicture(
                authorPubkeyHex: authorPubkeyHex,
                createdAt: createdAt,
                images: images.map { $0.toFfi() },
                post: post.toFfi()
            )
        )
    } catch let error as FfiPictureComposeError {
        throw PictureComposeError(error)
    }
}
