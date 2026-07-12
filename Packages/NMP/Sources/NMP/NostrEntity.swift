// The bech32 nostr-entity DECODE codec (#116) -- npub/nprofile/note/nevent/
// naddr -> hex id/pubkey + relay hints. A pure, stateless function: no
// `NMPEngine` instance is needed to call it, unlike everything else in this
// package.

import NMPFFI

/// A decoded public NIP-19 nostr entity (#116, `FfiNostrEntity` mirror).
/// Each case carries EXACTLY the fields NIP-19 defines for that entity --
/// never force-fit into one shared shape: `.pubkey`/`.eventId` carry no
/// relay hints at all (the format has none to carry); `.event`'s `author`/
/// `kind` are independently optional metadata; `.coordinate`'s `kind`/
/// `author`/`identifier` are ALL required by the format, unlike `.event`'s.
///
/// There is deliberately no `nsec`/`ncryptsec` case -- see
/// `decodeNostrEntity`'s doc for why a secret-key entity is refused rather
/// than decoded.
public enum NostrEntity: Sendable, Equatable {
    /// `npub` -- a bare public key (hex). No relay hints (the format
    /// carries none).
    case pubkey(pubkey: String)
    /// `nprofile` -- a public key (hex) plus zero or more relay hints.
    case profile(pubkey: String, relays: [String])
    /// `note` -- a bare event id (hex). No relay hints (the format carries
    /// none).
    case eventId(id: String)
    /// `nevent` -- an event id (hex) plus OPTIONAL author and/or kind
    /// (NIP-19: both are independently optional metadata, never implied by
    /// the id alone), plus zero or more relay hints.
    case event(id: String, author: String?, kind: UInt16?, relays: [String])
    /// `naddr` -- a parameterized-replaceable-event coordinate: `kind` +
    /// `author` (hex) + `d`-tag `identifier` (ALL required by the format,
    /// unlike `.event`'s optional author/kind), plus zero or more relay
    /// hints.
    case coordinate(kind: UInt16, author: String, identifier: String, relays: [String])

    init(_ ffi: FfiNostrEntity) {
        switch ffi {
        case .pubkey(let pubkey):
            self = .pubkey(pubkey: pubkey)
        case .profile(let pubkey, let relays):
            self = .profile(pubkey: pubkey, relays: relays)
        case .eventId(let id):
            self = .eventId(id: id)
        case .event(let id, let author, let kind, let relays):
            self = .event(id: id, author: author, kind: kind, relays: relays)
        case .coordinate(let kind, let author, let identifier, let relays):
            self = .coordinate(kind: kind, author: author, identifier: identifier, relays: relays)
        }
    }
}

/// Decode a bech32 nostr entity -- `npub`/`nprofile`/`note`/`nevent`/
/// `naddr` -- accepting either the bare bech32 string or a `nostr:`-
/// prefixed URI (NIP-21). Pure codec: no network, no signing, no engine
/// state -- callable with no `NMPEngine` constructed at all. Throws
/// `NMPError.nostrEntitySecretKeyRejected` rather than decoding an
/// `nsec`/`ncryptsec` -- a secret-key entity is never a valid target for a
/// display/mention codec.
public func decodeNostrEntity(_ bech32OrNostrURI: String) throws -> NostrEntity {
    try NostrEntity(nmpRethrowing { try NMPFFI.decodeNostrEntity(input: bech32OrNostrURI) })
}
