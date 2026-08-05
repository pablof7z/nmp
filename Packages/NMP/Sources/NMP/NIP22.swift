// Typed NIP-22 comments over NIP-73 external content ids (#572/#822/#1258). Demand,
// decode, and composition are pure protocol-owned functions. Composition
// returns NMP's ordinary `WriteIntent`; publication remains exclusively on
// `NMPEngine.publish`.

import NMPFFI

/// A validated NIP-73 external content id (`FfiNip73` mirror).
///
/// `.url` states the page a caller means; Rust normalises it (NIP-73's
/// table: "URL, normalized, no fragment"), so a value read back from a
/// decoded comment carries the canonical spelling rather than the one that
/// was sent. Normalising here as well would be a second owner of one rule.
public enum Nip73: Sendable, Hashable {
    case podcastEpisode(guid: String)
    case url(url: String)
    case general(value: String, kind: String)

    func toFfi() -> FfiNip73 {
        switch self {
        case .podcastEpisode(let guid): return .podcastEpisode(guid: guid)
        case .url(let url): return .url(url: url)
        case .general(let value, let kind): return .general(value: value, kind: kind)
        }
    }

    init(_ ffi: FfiNip73) {
        switch ffi {
        case .podcastEpisode(let guid): self = .podcastEpisode(guid: guid)
        case .url(let url): self = .url(url: url)
        case .general(let value, let kind): self = .general(value: value, kind: kind)
        }
    }
}

/// The root of a NIP-22 comment thread (`FfiCommentRoot` mirror). Every
/// comment in a thread, regardless of nesting depth, carries an IDENTICAL
/// root value.
public enum CommentRoot: Sendable, Hashable {
    case event(eventID: String, kind: UInt16, authorPubkey: String?)
    /// `eventID`: the addressable event's own id, when pinned alongside the
    /// coordinate (NIP-22: "when the parent event is replaceable or
    /// addressable, also include an `e`/`E` tag referencing its id"). `nil`
    /// remains a fully legal root.
    case address(authorPubkey: String, kind: UInt16, identifier: String, eventID: String?)
    case external(target: Nip73)

    func toFfi() -> FfiCommentRoot {
        switch self {
        case .event(let eventID, let kind, let authorPubkey):
            return .event(eventId: eventID, kind: kind, authorPubkey: authorPubkey)
        case .address(let authorPubkey, let kind, let identifier, let eventID):
            return .address(
                authorPubkey: authorPubkey, kind: kind, identifier: identifier, eventId: eventID
            )
        case .external(let target):
            return .external(target: target.toFfi())
        }
    }

    init(_ ffi: FfiCommentRoot) {
        switch ffi {
        case .event(let eventId, let kind, let authorPubkey):
            self = .event(eventID: eventId, kind: kind, authorPubkey: authorPubkey)
        case .address(let authorPubkey, let kind, let identifier, let eventId):
            self = .address(
                authorPubkey: authorPubkey, kind: kind, identifier: identifier, eventID: eventId
            )
        case .external(let target):
            self = .external(target: Nip73(target))
        }
    }
}

/// A comment's direct parent (`FfiCommentParent` mirror). `.root` means
/// this is a TOP-LEVEL comment (its parent mirrors the root); `.comment`
/// means it replies to another comment event.
public enum CommentParent: Sendable, Hashable {
    case root
    case comment(eventID: String, authorPubkey: String?)

    func toFfi() -> FfiCommentParent {
        switch self {
        case .root: return .root
        case .comment(let eventID, let authorPubkey):
            return .comment(eventId: eventID, authorPubkey: authorPubkey)
        }
    }

    init(_ ffi: FfiCommentParent) {
        switch ffi {
        case .root: self = .root
        case .comment(let eventId, let authorPubkey):
            self = .comment(eventID: eventId, authorPubkey: authorPubkey)
        }
    }
}

/// A successfully decoded, typed NIP-22 comment (`FfiDecodedComment`
/// mirror).
public struct DecodedComment: Sendable, Hashable {
    public let eventID: String
    public let authorPubkey: String
    public let createdAt: UInt64
    public let content: String
    public let root: CommentRoot
    public let parent: CommentParent

    init(_ ffi: FfiDecodedComment) {
        eventID = ffi.eventId
        authorPubkey = ffi.authorPubkey
        createdAt = ffi.createdAt
        content = ffi.content
        root = CommentRoot(ffi.root)
        parent = CommentParent(ffi.parent)
    }
}

/// `decodeComment`'s typed rejection (`FfiCommentDecodeError` mirror).
/// Exhaustive: malformed or mismatched tag sets stay raw rows, they never
/// become a typed comment.
public enum CommentDecodeError: Error, Sendable, Equatable {
    case wrongKind(got: UInt16)
    case missingRoot
    case duplicateContradictoryRoot
    case missingRootKind
    case invalidRootKind(got: String)
    case malformedRootReference
    case emptyExternalValue
    /// A `K`/`k` cell of `podcast:item:guid` declared an `I`/`i` value that
    /// did NOT carry the required `podcast:item:guid:` prefix.
    case malformedExternalValue(got: String)
    case missingParent
    case duplicateContradictoryParent
    case missingParentKind
    case invalidParentKind(got: String)
    case malformedParentReference
    case parentDoesNotMatchRootOrComment
    /// The delivered `Row`'s OWN `id`/`pubkey` envelope fields were not
    /// valid hex -- distinct from `.malformedRootReference`, which
    /// describes a root `E`/`A` TAG reference, never the row's own
    /// envelope.
    case malformedRowEnvelope(reason: String)

    init(_ ffi: FfiCommentDecodeError) {
        switch ffi {
        case .WrongKind(let got): self = .wrongKind(got: got)
        case .MissingRoot: self = .missingRoot
        case .DuplicateContradictoryRoot: self = .duplicateContradictoryRoot
        case .MissingRootKind: self = .missingRootKind
        case .InvalidRootKind(let got): self = .invalidRootKind(got: got)
        case .MalformedRootReference: self = .malformedRootReference
        case .EmptyExternalValue: self = .emptyExternalValue
        case .MalformedExternalValue(let got): self = .malformedExternalValue(got: got)
        case .MissingParent: self = .missingParent
        case .DuplicateContradictoryParent: self = .duplicateContradictoryParent
        case .MissingParentKind: self = .missingParentKind
        case .InvalidParentKind(let got): self = .invalidParentKind(got: got)
        case .MalformedParentReference: self = .malformedParentReference
        case .ParentDoesNotMatchRootOrComment: self = .parentDoesNotMatchRootOrComment
        case .MalformedRowEnvelope(let reason): self = .malformedRowEnvelope(reason: reason)
        }
    }
}

/// The demand for an entire NIP-22 comment thread rooted at `root`:
/// `kinds:[1111]`, scoped by the uppercase root reference on `#I`. One
/// filter covers the whole thread -- top-level comments AND every reply.
/// Throws `NMPError` if `root` fails to parse (e.g. a malformed pubkey/
/// event id hex, or an empty NIP-73 target cell).
public func commentThreadDemand(root: CommentRoot) throws -> NMPDemand {
    try NMPDemand(nmpRethrowing { try NMPFFI.commentThreadDemand(root: root.toFfi()) })
}

/// Decode a delivered kind:1111 `Row` into a typed `DecodedComment`.
/// Fallible: malformed or mismatched tag sets throw `CommentDecodeError`
/// and never become a typed comment.
public func decodeComment(_ row: Row) throws -> DecodedComment {
    let ffiRow = FfiRow(
        id: row.id, pubkey: row.pubkey, createdAt: row.createdAt, kind: row.kind,
        tags: row.tags, content: row.content, sig: row.sig, sources: row.sources
    )
    do {
        return try DecodedComment(NMPFFI.decodeComment(row: ffiRow))
    } catch let error as FfiCommentDecodeError {
        throw CommentDecodeError(error)
    }
}

/// Compose a durable, author-outbox-routed NIP-22 comment as NMP's ordinary
/// `WriteIntent` (#822). It names no author and reads no clock -- the engine
/// resolves the identity and stamps the time at acceptance -- so composition
/// still owns no engine state or lifecycle. `correlation` passes through
/// unchanged; publish the result through `NMPEngine.publish(_:)`.
public func commentIntent(
    root: CommentRoot,
    parent: CommentParent,
    content: String,
    correlation: String? = nil
) throws -> WriteIntent {
    try WriteIntent(
        nmpRethrowing {
            try NMPFFI.commentIntent(
                root: root.toFfi(),
                parent: parent.toFfi(),
                content: content,
                correlation: correlation
            )
        }
    )
}
