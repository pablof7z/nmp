import Foundation

/// App/protocol-owner supplied display fields for identity UI.
///
/// NMPUI does not decode kind:0. The exact profile protocol owner (#208)
/// may produce this presentation value after validating its own schema.
public struct NMPProfilePresentation: Sendable, Hashable {
    public let name: String?
    public let displayName: String?
    public let about: String?
    public let picture: String?
    public let banner: String?
    public let nip05: String?

    public init(
        name: String? = nil,
        displayName: String? = nil,
        about: String? = nil,
        picture: String? = nil,
        banner: String? = nil,
        nip05: String? = nil
    ) {
        self.name = name
        self.displayName = displayName
        self.about = about
        self.picture = picture
        self.banner = banner
        self.nip05 = nip05
    }
}

/// App/protocol-owner supplied display fields for article cards.
///
/// NMPUI does not decode NIP-23; the exact article owner is responsible for
/// producing this value from a validated event.
public struct NMPArticlePresentation: Sendable, Hashable {
    public let author: String
    public let createdAt: UInt64
    public let title: String?
    public let summary: String?
    public let image: String?
    public let publishedAt: UInt64?
    public let content: String

    public init(
        author: String,
        createdAt: UInt64,
        title: String? = nil,
        summary: String? = nil,
        image: String? = nil,
        publishedAt: UInt64? = nil,
        content: String
    ) {
        self.author = author
        self.createdAt = createdAt
        self.title = title
        self.summary = summary
        self.image = image
        self.publishedAt = publishedAt
        self.content = content
    }
}
