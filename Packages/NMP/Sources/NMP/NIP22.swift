// Typed NIP-22 comments over NIP-73 external targets (#572) -- pure
// functions, same shape as `NIP29.swift`'s precedent (#108): no `NMPEngine`
// instance is needed for root-thread demand or decode. `NMPEngine.
// commentIntent` (this file's write-side counterpart) needs no engine
// state either -- `nmp_nip22::comment_intent` takes author/time as
// explicit caller parameters -- but lives on `NMPEngine` for the same
// "engine door" naming symmetry as `groupMessageIntent`.

import NMPFFI

/// A validated NIP-73 external-content target (`FfiNip73Target` mirror).
public enum Nip73Target: Sendable, Hashable {
    case podcastEpisodeGuid(guid: String)
    case general(value: String, kind: String)

    func toFfi() -> FfiNip73Target {
        switch self {
        case .podcastEpisodeGuid(let guid): return .podcastEpisodeGuid(guid: guid)
        case .general(let value, let kind): return .general(value: value, kind: kind)
        }
    }

    init(_ ffi: FfiNip73Target) {
        switch ffi {
        case .podcastEpisodeGuid(let guid): self = .podcastEpisodeGuid(guid: guid)
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
    case external(target: Nip73Target)

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
            self = .external(target: Nip73Target(target))
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

/// A composed NIP-22 comment (#572), returned by `NMPEngine.commentIntent`.
/// Opaque and take-once -- pass it to `NMPEngine.publishComposed(_:)`
/// exactly once; a second attempt throws `NMPError.intentAlreadyConsumed`.
/// Never exposes the materialized tags, routing, author, or timestamp.
public struct CommentIntent: Sendable {
    let ffi: FfiComposedWriteIntent
}

extension NMPEngine {
    /// Compose a durable, author-outbox-routed NIP-22 comment `WriteIntent`
    /// (#572). Unlike `groupMessageIntent`, this needs no engine state at
    /// all -- author/time are explicit caller parameters -- but lives here
    /// for the same "engine door" naming symmetry. `correlation` (#591)
    /// passes straight through to `WriteIntent.correlation`. Publish the
    /// returned take-once value through `publishComposed(_:)`.
    public func commentIntent(
        root: CommentRoot,
        parent: CommentParent,
        authorPubkey: String,
        createdAt: UInt64,
        content: String,
        correlation: String? = nil
    ) throws -> CommentIntent {
        try CommentIntent(
            ffi: nmpRethrowing {
                try ffi.commentIntent(
                    root: root.toFfi(),
                    parent: parent.toFfi(),
                    authorPubkey: authorPubkey,
                    createdAt: createdAt,
                    content: content,
                    correlation: correlation
                )
            }
        )
    }
}
