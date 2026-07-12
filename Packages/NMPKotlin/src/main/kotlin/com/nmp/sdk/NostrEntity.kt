// The bech32 nostr-entity DECODE codec (#116) -- npub/nprofile/note/nevent/
// naddr -> hex id/pubkey + relay hints. A pure, stateless function: no
// `NMPEngine` instance is needed to call it, unlike everything else in this
// package. Mirrors NostrEntity.swift.

package com.nmp.sdk

import uniffi.nmp_ffi.FfiNostrEntity
import uniffi.nmp_ffi.decodeNostrEntity as ffiDecodeNostrEntity

/** A decoded public NIP-19 nostr entity (#116, `FfiNostrEntity` mirror).
 * Each subclass carries EXACTLY the fields NIP-19 defines for that entity --
 * never force-fit into one shared shape: `Pubkey`/`EventId` carry no relay
 * hints at all (the format has none to carry); `Event`'s `author`/`kind`
 * are independently optional metadata; `Coordinate`'s `kind`/`author`/
 * `identifier` are ALL required by the format, unlike `Event`'s.
 *
 * There is deliberately no `Nsec`/`NCryptSec` case -- see
 * `decodeNostrEntity`'s doc for why a secret-key entity is refused rather
 * than decoded. */
sealed class NostrEntity {
    /** `npub` -- a bare public key (hex). No relay hints (the format
     * carries none). */
    data class Pubkey(val pubkey: String) : NostrEntity()

    /** `nprofile` -- a public key (hex) plus zero or more relay hints. */
    data class Profile(val pubkey: String, val relays: List<String>) : NostrEntity()

    /** `note` -- a bare event id (hex). No relay hints (the format carries
     * none). */
    data class EventId(val id: String) : NostrEntity()

    /** `nevent` -- an event id (hex) plus OPTIONAL author and/or kind
     * (NIP-19: both are independently optional metadata, never implied by
     * the id alone), plus zero or more relay hints. */
    data class Event(
        val id: String,
        val author: String?,
        val kind: UShort?,
        val relays: List<String>,
    ) : NostrEntity()

    /** `naddr` -- a parameterized-replaceable-event coordinate: `kind` +
     * `author` (hex) + `d`-tag `identifier` (ALL required by the format,
     * unlike `Event`'s optional author/kind), plus zero or more relay
     * hints. */
    data class Coordinate(
        val kind: UShort,
        val author: String,
        val identifier: String,
        val relays: List<String>,
    ) : NostrEntity()

    companion object {
        fun from(ffi: FfiNostrEntity): NostrEntity =
            when (ffi) {
                is FfiNostrEntity.Pubkey -> Pubkey(ffi.pubkey)
                is FfiNostrEntity.Profile -> Profile(ffi.pubkey, ffi.relays)
                is FfiNostrEntity.EventId -> EventId(ffi.id)
                is FfiNostrEntity.Event -> Event(ffi.id, ffi.author, ffi.kind, ffi.relays)
                is FfiNostrEntity.Coordinate ->
                    Coordinate(ffi.kind, ffi.author, ffi.identifier, ffi.relays)
            }
    }
}

/** Decode a bech32 nostr entity -- `npub`/`nprofile`/`note`/`nevent`/
 * `naddr` -- accepting either the bare bech32 string or a `nostr:`-prefixed
 * URI (NIP-21). Pure codec: no network, no signing, no engine state --
 * callable with no `NMPEngine` constructed at all. Throws
 * `NMPError.NostrEntitySecretKeyRejected` rather than decoding an
 * `nsec`/`ncryptsec` -- a secret-key entity is never a valid target for a
 * display/mention codec. */
fun decodeNostrEntity(bech32OrNostrURI: String): NostrEntity =
    NostrEntity.from(nmpRethrowing { ffiDecodeNostrEntity(bech32OrNostrURI) })
