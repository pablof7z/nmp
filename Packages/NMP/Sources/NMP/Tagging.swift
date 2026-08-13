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

    /// State what this draft SAYS, and emit the rows its inline references
    /// need, from one call.
    ///
    /// A composed draft is content-free until the app says what it says, so
    /// this is how a draft from one of these doors becomes a message. It takes
    /// the message in PIECES rather than as a finished string because a piece
    /// naming a person or an event produces both halves of that reference —
    /// the `nostr:npub1…`/`nostr:nevent1…` a reader sees and the `p`/`q` row
    /// that resolves it — so the two cannot be written apart. Writing them
    /// apart is what #964 found still living in Swift: an app that let
    /// somebody @-mention a person appended `["p", hex]` by hand and hoped it
    /// matched the token it had put in the content, and nothing could catch a
    /// disagreement, because from the app's side nothing is missing.
    ///
    /// The rows land after whatever the composer already stated for its own
    /// reasons — a chat reply's `e` and `p` rows survive intact.
    ///
    /// A pre-signed payload is returned unchanged: its content is frozen in
    /// bytes that were already signed over, so changing it would invalidate
    /// the signature rather than edit the message.
    public func withContent(_ content: [ContentPart]) throws -> WritePayload {
        switch self {
        case .event(let kind, let tags, let stated, let createdAt):
            return WritePayload(
                try nmpRethrowing {
                    try NMPFFI.withContent(
                        draft: FfiEventBuilder(
                            kind: kind, tags: tags, content: stated, createdAt: createdAt),
                        content: content.map { $0.toFfi() })
                })
        case .signed:
            return self
        }
    }
}

/// One piece of a message body.
///
/// Bech32 appears in what a reader SEES and nowhere else — that is the user
/// boundary (`docs/internals/conventions/bech32-boundary.md`). Every input
/// here is the decoded form: `pubkey` is 64-char hex like every other key in
/// this package, and a quote names the `Row` the app is already holding. The
/// `nostr:npub1…`/`nostr:nevent1…` token is produced from those, which is
/// exactly the pairing this type exists to keep honest.
public enum ContentPart: Sendable, Hashable {
    /// Literal text, rendered verbatim and emitting no rows. A `nostr:` URI
    /// typed into this case is just characters: nothing parses it, so it
    /// emits nothing. Name the person or the event instead.
    case text(String)
    /// Somebody named inline. Renders `nostr:npub1…` and emits their `p` row.
    ///
    /// `relay` is where a reader should look for them, when the app knows —
    /// a person's relay is an outbox fact (NIP-65) no schema owner can reach,
    /// so `nil` leaves the slot honestly empty rather than guessing. Stating
    /// one reaches both halves: the rendering becomes `nostr:nprofile1…`
    /// carrying that relay, and the `p` row's hint cell carries the same
    /// value.
    case person(pubkey: String, relay: String?)
    /// An event named inline. Renders `nostr:nevent1…` and emits its NIP-18
    /// `q` row, hinted from the row's own verified sources.
    ///
    /// It is a QUOTE and never a thread reply: NIP-18's `q` exists precisely
    /// so *"quote reposts are not pulled and included as replies in threads"*.
    /// Replying is `chatReply(to:)`/`replyTo(_:)`, which point with `e`.
    // nmp-native:if nip18
    case quote(Row)
    // nmp-native:endif

    func toFfi() -> FfiContentPart {
        switch self {
        case .text(let text): return .text(text: text)
        case .person(let pubkey, let relay): return .person(pubkey: pubkey, relay: relay)
        // nmp-native:if nip18
        case .quote(let target): return .quote(target: target.toFfi())
        // nmp-native:endif
        }
    }
}

// nmp-native:if nipc7
/// Compose a top-level NIP-C7 kind:9 chat.
///
/// The other half of what `chatReply(to:)` closed: an app that replies no
/// longer states a kind, but an app sending an ordinary message still stated
/// `kind: 9` itself, because the composer for THAT never crossed the FFI
/// (#964).
///
/// It composes SCHEMA ONLY, exactly as `chatReply(to:)` does — no `h` row, no
/// notification policy, no routing, and no content. What the message says
/// comes from `withContent(_:)`, which is also what emits the rows an inline
/// mention or quote needs. A group's `h` row and its relay set come from
/// `NMPGroup.publish(engine:authorPubkeyHex:payload:)`.
public func chat() -> WritePayload {
    WritePayload(NMPFFI.chat())
}
// nmp-native:endif

// nmp-native:if nip22
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
// nmp-native:endif

// nmp-native:if nipc7
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
// nmp-native:endif

// nmp-native:if nip18
/// Compose a NIP-18 repost of `target`.
///
/// NIP-18 owns both kinds, so the two-way split happens inside it: a reposted
/// text note is a kind:6 and anything else is a kind:16 that states what it
/// reposted. A caller never picks a kind.
public func repost(_ target: Row) throws -> WritePayload {
    try WritePayload(nmpRethrowing { try NMPFFI.repost(target: target.toFfi()) })
}
// nmp-native:endif

// nmp-native:if nip25
/// What a NIP-25 reaction says.
///
/// Not a string, because NIP-25 assigns fixed meanings to fixed bytes: content
/// of `+` *or the empty string* MUST be read as a like, and `-` MUST be read
/// as a dislike. An app writing content by hand can therefore spell "like"
/// three ways, and can spell it by accident when an emoji picker returns
/// nothing. These are the spec's own three readings and there is no fourth.
public enum Reaction: Sendable, Hashable {
    /// Rendered `+`.
    case like
    /// Rendered `-`.
    case dislike
    /// An emoji, which NIP-25 says SHOULD NOT be read as a like or a dislike.
    ///
    /// Validated by `react(to:with:)`: the empty string throws, because NIP-25
    /// reads it as a like, and a NIP-30 `:shortcode:` throws, because it needs
    /// a companion `emoji` row this door does not write and would otherwise
    /// reach every reader as literal colons.
    case emoji(String)

    func toFfi() -> FfiReaction {
        switch self {
        case .like: return .like
        case .dislike: return .dislike
        case .emoji(let emoji): return .emoji(emoji: emoji)
        }
    }
}

/// Compose a NIP-25 reaction to `target`.
///
/// NMP had no reaction door at all, so both consuming apps hand-wrote
/// `kind: 7` with their own `["e", …]` and `["p", …]` rows (#155). What that
/// spelling loses is not the kind — it is everything this door fills: the
/// relay hint NMP actually observed, the author slot, the `k` row naming what
/// was reacted to, and the fact that a reaction to a REPLY must name the reply
/// rather than its thread root, so a client tallying by the first `e` cannot
/// credit the root with a reaction nobody gave it.
///
/// It composes SCHEMA ONLY — no routing, no identity, no `h` row. A group's
/// `h` row and its relay set come from
/// `NMPGroup.publish(engine:authorPubkeyHex:payload:)`, which takes exactly
/// this value.
public func react(to target: Row, with reaction: Reaction) throws -> WritePayload {
    try WritePayload(
        nmpRethrowing {
            try NMPFFI.reactTo(target: target.toFfi(), reaction: reaction.toFfi())
        })
}
// nmp-native:endif

extension Row {
    func toFfi() -> FfiRow {
        FfiRow(
            id: id, pubkey: pubkey, createdAt: createdAt, kind: kind,
            tags: tags, content: content, signature: signature.ffi, sources: sources
        )
    }
}
