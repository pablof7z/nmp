// Opaque typed NIP-25 target/draft projection (#155). Target qualification
// re-reads the canonical Rust store by event id; the caller's raw Row fields
// and sources are not trusted inputs. Draft bytes stay opaque for a closed
// publisher such as the upcoming NIP-29 publication gate.

import NMPFFI

/// One validated semantic NIP-25 reaction value.
public enum ReactionValue: Sendable, Hashable {
    case like
    case dislike
    case emoji(String)
    case customEmoji(shortcode: String, imageURL: String)

    func toFfi() -> FfiReactionValue {
        switch self {
        case .like:
            return .like
        case .dislike:
            return .dislike
        case .emoji(let value):
            return .emoji(value: value)
        case .customEmoji(let shortcode, let imageURL):
            return .customEmoji(shortcode: shortcode, imageUrl: imageURL)
        }
    }
}

/// Opaque capability proving that NMP qualified one canonical signed event
/// as a complete native-event reaction target.
public final class ReactionTarget: @unchecked Sendable {
    let ffi: FfiReactionTarget

    init(_ ffi: FfiReactionTarget) {
        self.ffi = ffi
    }
}

/// Opaque immutable unsigned protocol draft. It exposes no event kind, tags,
/// author, time, routing, signing, receipt, retry, or publication operation.
public final class ProtocolDraft: @unchecked Sendable {
    let ffi: FfiProtocolDraft

    init(_ ffi: FfiProtocolDraft) {
        self.ffi = ffi
    }
}

/// Typed failures from NIP-25 target qualification and draft composition.
public enum ReactionError: Error, Sendable, Equatable {
    case invalidEventID(got: String)
    case targetNotFound(eventID: String)
    case targetNotVerified(eventID: String)
    case canonicalLookupUnavailable(reason: String)
    case engineClosed
    case noActiveReactionAuthor
    case emptyEmoji
    case standardValueRequiresTypedVariant(got: String)
    case customEmojiRequiresMetadata(got: String)
    case invalidEmojiToken(got: String)
    case invalidCustomEmojiShortcode(got: String)
    case invalidCustomEmojiURL(got: String)

    init(_ ffi: FfiReactionError) {
        switch ffi {
        case .InvalidEventId(let got):
            self = .invalidEventID(got: got)
        case .TargetNotFound(let eventId):
            self = .targetNotFound(eventID: eventId)
        case .TargetNotVerified(let eventId):
            self = .targetNotVerified(eventID: eventId)
        case .CanonicalLookupUnavailable(let reason):
            self = .canonicalLookupUnavailable(reason: reason)
        case .EngineClosed:
            self = .engineClosed
        case .NoActiveReactionAuthor:
            self = .noActiveReactionAuthor
        case .EmptyEmoji:
            self = .emptyEmoji
        case .StandardValueRequiresTypedVariant(let got):
            self = .standardValueRequiresTypedVariant(got: got)
        case .CustomEmojiRequiresMetadata(let got):
            self = .customEmojiRequiresMetadata(got: got)
        case .InvalidEmojiToken(let got):
            self = .invalidEmojiToken(got: got)
        case .InvalidCustomEmojiShortcode(let got):
            self = .invalidCustomEmojiShortcode(got: got)
        case .InvalidCustomEmojiUrl(let got):
            self = .invalidCustomEmojiURL(got: got)
        }
    }
}

extension NMPEngine {
    /// Qualify `row.id` through NMP's canonical cache. Every other field on
    /// this caller-constructible row, including `sources`, is ignored.
    public func reactionTarget(for row: Row) throws -> ReactionTarget {
        do {
            return try ReactionTarget(ffi.reactionTarget(eventId: row.id))
        } catch let error as FfiReactionError {
            throw ReactionError(error)
        }
    }

    /// Compose one Rust-authored unsigned NIP-25 draft using NMP's active
    /// account and Rust-owned time. This does not publish the event.
    public func reactionDraft(
        target: ReactionTarget,
        value: ReactionValue
    ) throws -> ProtocolDraft {
        do {
            return try ProtocolDraft(
                ffi.reactionDraft(target: target.ffi, value: value.toFfi())
            )
        } catch let error as FfiReactionError {
            throw ReactionError(error)
        }
    }
}
