import NMP
import NMPFFI

/// Source-faithful kind:0 fields. Display-name and placeholder policy stays in
/// the native component layer.
public struct NostrProfileMetadata: Sendable, Hashable {
    public let pubkey: String
    public let name: String?
    public let displayName: String?
    public let about: String?
    public let picture: String?
    public let banner: String?
    public let nip05: String?
    public let lud06: String?
    public let lud16: String?

    public init(
        pubkey: String,
        name: String? = nil,
        displayName: String? = nil,
        about: String? = nil,
        picture: String? = nil,
        banner: String? = nil,
        nip05: String? = nil,
        lud06: String? = nil,
        lud16: String? = nil
    ) {
        self.pubkey = pubkey
        self.name = name
        self.displayName = displayName
        self.about = about
        self.picture = picture
        self.banner = banner
        self.nip05 = nip05
        self.lud06 = lud06
        self.lud16 = lud16
    }
}

/// Typed NIP-23 long-form event fields. Reading-time estimation is deliberately
/// not a protocol field and belongs to the UI composition.
public struct NostrArticle: Sendable, Hashable {
    public let eventID: String
    public let author: String
    public let createdAt: UInt64
    public let identifier: String
    public let title: String?
    public let summary: String?
    public let image: String?
    public let publishedAt: UInt64?
    public let content: String

    public init(
        eventID: String,
        author: String,
        createdAt: UInt64,
        identifier: String,
        title: String? = nil,
        summary: String? = nil,
        image: String? = nil,
        publishedAt: UInt64? = nil,
        content: String
    ) {
        self.eventID = eventID
        self.author = author
        self.createdAt = createdAt
        self.identifier = identifier
        self.title = title
        self.summary = summary
        self.image = image
        self.publishedAt = publishedAt
        self.content = content
    }
}

public func decodeNostrProfile(from row: Row) -> NostrProfileMetadata? {
    guard row.kind == 0 else { return nil }
    let value = decodeProfileResource(row: row.ffiValue)
    return NostrProfileMetadata(value)
}

public func decodeNIP23Article(from row: Row) -> NostrArticle? {
    guard row.kind == 30_023 else { return nil }
    return NostrArticle(decodeArticleResource(row: row.ffiValue))
}

extension Row {
    fileprivate var ffiValue: FfiRow {
        FfiRow(
            id: id,
            pubkey: pubkey,
            createdAt: createdAt,
            kind: kind,
            tags: tags,
            content: content,
            sig: sig,
            sources: sources
        )
    }
}

extension NostrProfileMetadata {
    fileprivate init(_ ffi: FfiProfileMetadata) {
        self.init(
            pubkey: ffi.pubkey,
            name: ffi.name,
            displayName: ffi.displayName,
            about: ffi.about,
            picture: ffi.picture,
            banner: ffi.banner,
            nip05: ffi.nip05,
            lud06: ffi.lud06,
            lud16: ffi.lud16
        )
    }
}

extension NostrArticle {
    fileprivate init(_ ffi: FfiArticle) {
        self.init(
            eventID: ffi.eventId,
            author: ffi.author,
            createdAt: ffi.createdAt,
            identifier: ffi.identifier,
            title: ffi.title,
            summary: ffi.summary,
            image: ffi.image,
            publishedAt: ffi.publishedAt,
            content: ffi.content
        )
    }
}
