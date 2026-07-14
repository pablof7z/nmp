import NMPFFI

public enum RelayInformationCachePolicy: Sendable {
    case useCache
    case refresh

    func toFfi() -> FfiRelayInformationCachePolicy {
        switch self {
        case .useCache: return .useCache
        case .refresh: return .refresh
        }
    }
}

public enum RelayInformationFreshness: Sendable, Equatable {
    case fresh
    case stale

    init(_ ffi: FfiRelayInformationFreshness) {
        switch ffi {
        case .fresh: self = .fresh
        case .stale: self = .stale
        }
    }
}

/// Advisory limits claimed by the relay. Omitted fields remain `nil`; they
/// are never inferred as zero/false or treated as runtime proof.
public struct RelayInformationLimitations: Sendable, Equatable {
    public let maxMessageLength: UInt64?
    public let maxSubscriptions: UInt64?
    public let maxFilters: UInt64?
    public let maxLimit: UInt64?
    public let maxSubidLength: UInt64?
    public let maxEventTags: UInt64?
    public let maxContentLength: UInt64?
    public let minPowDifficulty: UInt64?
    public let authRequired: Bool?
    public let paymentRequired: Bool?
    public let createdAtLowerLimit: UInt64?
    public let createdAtUpperLimit: UInt64?

    init(_ ffi: FfiRelayInformationLimitations) {
        maxMessageLength = ffi.maxMessageLength
        maxSubscriptions = ffi.maxSubscriptions
        maxFilters = ffi.maxFilters
        maxLimit = ffi.maxLimit
        maxSubidLength = ffi.maxSubidLength
        maxEventTags = ffi.maxEventTags
        maxContentLength = ffi.maxContentLength
        minPowDifficulty = ffi.minPowDifficulty
        authRequired = ffi.authRequired
        paymentRequired = ffi.paymentRequired
        createdAtLowerLimit = ffi.createdAtLowerLimit
        createdAtUpperLimit = ffi.createdAtUpperLimit
    }
}

public struct RelayInformationDocument: Sendable, Equatable {
    public let name: String?
    public let description: String?
    public let banner: String?
    public let icon: String?
    public let pubkey: String?
    public let selfPubkey: String?
    public let contact: String?
    /// `nil` means no list was advertised; an empty array is an explicit
    /// advertisement of no supported NIPs.
    public let supportedNips: [UInt16]?
    public let software: String?
    public let version: String?
    public let termsOfService: String?
    public let limitation: RelayInformationLimitations
    public let structured: [String: String]

    init(_ ffi: FfiRelayInformationDocument) {
        name = ffi.name
        description = ffi.description
        banner = ffi.banner
        icon = ffi.icon
        pubkey = ffi.pubkey
        selfPubkey = ffi.selfPubkey
        contact = ffi.contact
        supportedNips = ffi.supportedNips
        software = ffi.software
        version = ffi.version
        termsOfService = ffi.termsOfService
        limitation = RelayInformationLimitations(ffi.limitation)
        structured = ffi.structured
    }
}

/// A last-good NIP-11 representation. `rawJSON` preserves unknown future
/// fields; `lastError` is separate stale-on-error evidence.
public struct RelayInformation: Sendable, Equatable {
    public let relay: String
    public let document: RelayInformationDocument
    public let rawJSON: String
    public let documentRevision: String
    public let fetchedAt: UInt64
    public let freshUntil: UInt64
    public let freshness: RelayInformationFreshness
    public let etag: String?
    public let lastModified: String?
    public let cacheControl: String?
    public let expires: String?
    public let lastError: String?

    init(_ ffi: FfiRelayInformation) {
        relay = ffi.relay
        document = RelayInformationDocument(ffi.document)
        rawJSON = ffi.rawJson
        documentRevision = ffi.documentRevision
        fetchedAt = ffi.fetchedAt
        freshUntil = ffi.freshUntil
        freshness = RelayInformationFreshness(ffi.freshness)
        etag = ffi.etag
        lastModified = ffi.lastModified
        cacheControl = ffi.cacheControl
        expires = ffi.expires
        lastError = ffi.lastError
    }
}
