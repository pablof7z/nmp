// The tagging door (#1243): compose a reply, a chat reply or a repost by
// naming what you are pointing at, and nothing else.
//
// This is what #1243 asked for. A native NIP-29 chat app could reach
// `NMPGroup.publish` for the `h` row but had to hand-build the one row NIP-C7
// owns, because the C7 composer never crossed the FFI:
//
//     /// NOT NMP-owned yet, and it should be
//     tags.append(["q", reply.eventID, relay, reply.author.pubkey])
//
// That row was wrong twice over -- a `q` is NIP-18's QUOTE marker, whose whole
// purpose is keeping the referenced event OUT of the thread -- and nothing
// caught it, which is exactly why schema ownership sits in NMP.
//
// Every door here takes the `Row` the app is already holding and returns a
// `WritePayload`, the same value `NMPEngine.publish(_:)` and
// `NMPGroup.publish(engine:authorPubkeyHex:payload:)` already take. None of
// them takes a relationship, a marker, a relay hint or an author: those are
// what the door fills, from the row's own tags and its verified sources.

import NMPFFI

extension WritePayload {
    init(_ ffi: FfiEventBuilder) {
        self = .event(
            kind: ffi.kind, tags: ffi.tags, content: ffi.content, createdAt: ffi.createdAt)
    }

    /// This payload's content, restated. A composed draft is content-free
    /// until the app says what it says, so `withContent(_:)` is how a draft
    /// from one of these doors becomes a message.
    public func withContent(_ content: String) -> WritePayload {
        switch self {
        case .event(let kind, let tags, _, let createdAt):
            return .event(kind: kind, tags: tags, content: content, createdAt: createdAt)
        case .signed:
            // A signed event's content is frozen in bytes that were already
            // signed over; changing it would invalidate the signature rather
            // than edit the message.
            return self
        }
    }
}

/// Compose the ordinary reply to `target`.
///
/// Two-way and no more: a text note threads through NIP-10, and everything
/// else becomes a NIP-22 comment. The split reads the TARGET's kind, and the
/// root/parent determination underneath reads neither the target's kind nor
/// the kind being composed — it reads the target's own rows. So a reply
/// composed by an app that believes it is replying to a thread root and one
/// composed by an app that knows better produce the same rows, which is the
/// inversion amethyst#629 shipped and this makes unspellable.
public func replyTo(_ target: Row) throws -> WritePayload {
    try WritePayload(nmpRethrowing { try NMPFFI.replyTo(target: target.toFfi()) })
}

/// Compose a NIP-C7 kind:9 chat reply to `target`.
///
/// C7 offers its own verb rather than an arm in the general dispatcher
/// because kind:9 must NOT become a NIP-22 comment: NIP-29 clients MUST only
/// fetch kind 9, so a 1111 reply inside a group would be invisible to every
/// one of them. The reply row is `e`, not `q`.
///
/// It composes SCHEMA ONLY — no `h` row, no notification policy, no routing.
/// A group's `h` row and its relay set come from
/// `NMPGroup.publish(engine:authorPubkeyHex:payload:)`, which takes exactly
/// this value.
public func chatReply(to target: Row) throws -> WritePayload {
    try WritePayload(nmpRethrowing { try NMPFFI.chatReply(target: target.toFfi()) })
}

/// Compose a NIP-18 repost of `target`.
///
/// NIP-18 owns both kinds, so the two-way split happens inside it: a reposted
/// text note is a kind:6 and anything else is a kind:16 that states what it
/// reposted. A caller never picks a kind.
public func repost(_ target: Row) throws -> WritePayload {
    try WritePayload(nmpRethrowing { try NMPFFI.repost(target: target.toFfi()) })
}

extension Row {
    func toFfi() -> FfiRow {
        FfiRow(
            id: id, pubkey: pubkey, createdAt: createdAt, kind: kind,
            tags: tags, content: content, sig: sig, sources: sources
        )
    }
}
