// The bech32 nostr-entity decode codec (#116) -- construction/mapping tests
// only. No network: bech32 decoding is pure and local.

package com.nmp.sdk

import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

class NostrEntityTest {
    @Test
    fun decodesABareNpub() {
        val npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
        val entity = decodeNostrEntity(npub) as NostrEntity.Pubkey
        assertEquals("aa4fc8665f5696e33db7e1a572e3b0f5b3d615837b0f362dcb1c8068b098c7b4", entity.pubkey)
    }

    @Test
    fun decodesANostrUriPrefixedEntityIdenticallyToTheBareForm() {
        val npub = "npub14f8usejl26twx0dhuxjh9cas7keav9vr0v8nvtwtrjqx3vycc76qqh9nsy"
        assertEquals(decodeNostrEntity("nostr:$npub"), decodeNostrEntity(npub))
    }

    @Test
    fun decodesAnNprofileWithRelayHints() {
        val nprofile =
            "nprofile1qqsrhuxx8l9ex335q7he0f09aej04zpazpl0ne2cgukyawd24mayt8gppemhxue69uhhytnc9e3k7mf0qyt8wumn8ghj7er2vfshxtnnv9jxkc3wvdhk6tclr7lsh"
        val entity = decodeNostrEntity(nprofile) as NostrEntity.Profile
        assertEquals("3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d", entity.pubkey)
        assertEquals(listOf("wss://r.x.com/", "wss://djbas.sadkb.com/"), entity.relays)
    }

    /** #116's honest-modeling requirement: `Event`'s `author`/`kind` stay
     * independently optional, never force-fit. */
    @Test
    fun decodesAnNeventWithAnAuthorButNoKindOrRelays() {
        val nevent =
            "nevent1qqsdhet4232flykq3048jzc9msmaa3hnxuesxy3lnc33vd0wt9xwk6szyqewrqnkx4zsaweutf739s0cu7et29zrntqs5elw70vlm8zudr3y24sqsgy"
        val entity = decodeNostrEntity(nevent) as NostrEntity.Event
        assertEquals("dbe57554549f92c08bea790b05dc37dec6f3373303123f9e231635ee594ceb6a", entity.id)
        assertEquals("32e1827635450ebb3c5a7d12c1f8e7b2b514439ac10a67eef3d9fd9c5c68e245", entity.author)
        assertNull(entity.kind)
        assertEquals(emptyList<String>(), entity.relays)
    }

    @Test
    fun decodesAnNaddrWithKindAuthorAndIdentifier() {
        val naddr =
            "naddr1qqxnzd3exgersv33xymnsve3qgs8suecw4luyht9ekff89x4uacneapk8r5dyk0gmn6uwwurf6u9rusrqsqqqa282m3gxt"
        val entity = decodeNostrEntity(naddr) as NostrEntity.Coordinate
        assertEquals(30023.toUShort(), entity.kind)
        assertEquals("787338757fc25d65cd929394d5e7713cf43638e8d259e8dcf5c73b834eb851f2", entity.author)
        assertEquals("1692282117831", entity.identifier)
        assertEquals(emptyList<String>(), entity.relays)
    }

    /** The refusal case: a secret-key entity must never decode as if it
     * were a display target. */
    @Test
    fun rejectsAnNsecRatherThanLeakingTheSecretKey() {
        val nsec = "nsec1j4c6269y9w0q2er2xjw8sv2ehyrtfxq3jwgdlxj6qfn8z4gjsq5qfvfk99"
        assertThrows(NMPError.NostrEntitySecretKeyRejected::class.java) {
            decodeNostrEntity(nsec)
        }
    }

    @Test
    fun rejectsMalformedBech32WithATypedErrorNeverACrash() {
        assertThrows(NMPError.InvalidNostrEntity::class.java) {
            decodeNostrEntity("npub1notvalidbech32")
        }
    }
}
