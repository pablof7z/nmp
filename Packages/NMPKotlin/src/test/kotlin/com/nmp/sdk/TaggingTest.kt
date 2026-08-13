// #1243: the tagging door at the native boundary. Mirrors TaggingTests.swift.

package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertFalse
import kotlin.test.assertTrue

class TaggingTest {
    private fun row(
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        sources: List<String> = emptyList(),
    ) = Row(
        id = "a".repeat(64),
        pubkey = "b".repeat(64),
        createdAt = 1_700_000_000UL,
        kind = kind,
        tags = tags,
        content = "body",
        signature = RowSignature.Signed("c".repeat(128)),
        sources = sources,
    )

    private fun event(payload: WritePayload): WritePayload.Event =
        payload as? WritePayload.Event
            ?: error("the tagging door composes an ordinary builder payload")

    /** #1243's own report, closed at the boundary it named: a native chat app
     * composes a C7 reply through NMP instead of hand-building a row, it is
     * kind:9, and it points with `e` rather than NIP-18's `q` quote marker. */
    @Test
    fun chatReplyIsKindNineAndPointsWithE() {
        val parent = row(9u, sources = listOf("wss://chat.example.com"))
        val composed = event(chatReply(parent))
        assertEquals(9u.toUShort(), composed.kind)

        val eRow = composed.tags.first { it[0] == "e" }
        assertEquals(parent.id, eRow[1])
        assertEquals("wss://chat.example.com", eRow[2])
        assertEquals(parent.pubkey, eRow[3])
        assertFalse(composed.tags.any { it[0] == "q" }, "a reply is not a quote")
        assertFalse(composed.tags.any { it[0] == "h" }, "group context is NIP-29's, never C7's")
        assertTrue(composed.tags.any { it[0] == "p" && it[1] == parent.pubkey })
    }

    /** The thread position is the wire's, across the boundary as much as in
     * Rust: replying to a reply names the ROOT as root and the target as
     * reply, whatever the app believed about either. */
    @Test
    fun replyReadsTheTargetsOwnThreadPosition() {
        val rootId = "d".repeat(64)
        val target = row(1u, tags = listOf(listOf("e", rootId, "", "root")))
        val composed = event(replyTo(target))
        assertEquals(1u.toUShort(), composed.kind)
        assertEquals(rootId, composed.tags[0][1])
        assertEquals("root", composed.tags[0][3])
        assertEquals(target.id, composed.tags[1][1])
        assertEquals("reply", composed.tags[1][3])
    }

    /** A repost names the entity, so reposting a reply reposts THAT note and
     * never the conversation's root -- which is what a NIP-18 reader would
     * otherwise take from a threaded row pair, since it reads the first `e`. */
    @Test
    fun repostNamesTheEntityAndSplitsItsOwnKind() {
        val rootId = "e".repeat(64)
        val reply = row(1u, tags = listOf(listOf("e", rootId, "", "root")))
        val composed = event(repost(reply))
        assertEquals(6u.toUShort(), composed.kind)
        val eRows = composed.tags.filter { it[0] == "e" }
        assertEquals(1, eRows.size)
        assertEquals(reply.id, eRows[0][1])

        val generic = event(repost(row(20u)))
        assertEquals(16u.toUShort(), generic.kind)
        assertTrue(generic.tags.any { it[0] == "k" && it[1] == "20" })
    }

    /** A composed draft is content-free until the app says what it says. */
    @Test
    fun withContentFillsADraftWithoutDisturbingItsRows() {
        val draft = chatReply(row(9u))
        val bare = event(draft)
        val filled = event(draft.withContent(listOf(ContentPart.Text("hello"))))
        assertEquals("", bare.content)
        assertEquals("hello", filled.content)
        assertEquals(bare.tags, filled.tags)
    }

    /** #964's remaining half: a message that is NOT a reply. Until this door
     * crossed the boundary an app stated `kind: 9` itself for every ordinary
     * message it sent. */
    @Test
    fun chatIsKindNineAndCarriesNoRows() {
        val composed = event(chat())
        assertEquals(9u.toUShort(), composed.kind)
        assertTrue(composed.tags.isEmpty(), "a chat states no policy rows")
        assertEquals("", composed.content)
        assertEquals(null, composed.createdAt, "a schema-only composer invents no timestamp")
    }

    /** The whole point of the door: the `nostr:npub...` a reader sees and the
     * `p` row that notifies the person come out of ONE statement, so an app can
     * no longer append `["p", hex]` by hand and hope it matches the token it
     * separately put in the content. */
    @Test
    fun namingAPersonWritesTheTokenAndThePRowTogether() {
        val alice = "b".repeat(64)
        val composed = event(
            chat().withContent(
                listOf(
                    ContentPart.Text("hey "),
                    ContentPart.Person(alice, null),
                    ContentPart.Text(", look"),
                ),
            ),
        )
        assertTrue(
            composed.content.startsWith("hey nostr:npub1"),
            "bech32 is rendered at the user boundary: ${composed.content}",
        )
        assertTrue(composed.content.endsWith(", look"))
        assertEquals(listOf(listOf("p", alice)), composed.tags)
    }

    /** A stated relay reaches BOTH halves, because both come from the same
     * part: the rendered pointer becomes an `nprofile` carrying the relay and
     * the `p` row's hint cell carries the same value. */
    @Test
    fun aStatedRelayReachesTheTokenAndTheRowTogether() {
        val alice = "b".repeat(64)
        val composed = event(
            chat().withContent(listOf(ContentPart.Person(alice, "wss://relay.example"))),
        )
        assertTrue(composed.content.startsWith("nostr:nprofile1"), composed.content)
        assertEquals(listOf(listOf("p", alice, "wss://relay.example")), composed.tags)
    }

    /** An event named inline is a QUOTE, never a thread reply, and its hint
     * comes from where NMP actually saw it -- the row's own verified sources. */
    @Test
    fun quotingAnEventRendersItAndEmitsItsQRow() {
        val quoted = row(9u, sources = listOf("wss://chat.example.com"))
        val composed = event(
            chat().withContent(listOf(ContentPart.Text("look: "), ContentPart.Quote(quoted))),
        )
        assertTrue(composed.content.startsWith("look: nostr:nevent1"), composed.content)
        assertEquals(
            listOf(listOf("q", quoted.id, "wss://chat.example.com", quoted.pubkey)),
            composed.tags,
        )
    }

    /** #155's own report, closed at the boundary it named: a native app
     * composes a reaction through NMP instead of hand-writing `kind: 7` with its
     * own `e` and `p` rows, and the door fills the hint, the author slot and the
     * `k` row an app-written pair never carried. */
    @Test
    fun reactionIsKindSevenAndCarriesWhatTheOneDoorFills() {
        val target = row(1u, sources = listOf("wss://relay.example"))
        val composed = event(react(target, Reaction.Like))
        assertEquals(7u.toUShort(), composed.kind)
        assertEquals("+", composed.content)

        val eRow = composed.tags.first { it[0] == "e" }
        assertEquals(target.id, eRow[1])
        assertEquals("wss://relay.example", eRow[2])
        assertEquals(target.pubkey, eRow[3])
        assertTrue(composed.tags.any { it[0] == "p" && it[1] == target.pubkey })
        assertTrue(composed.tags.any { it[0] == "k" && it[1] == "1" })
    }

    /** The three readings NIP-25 defines. An app never writes the content
     * bytes, so it cannot spell "like" by accident. */
    @Test
    fun theReactionVocabularyIsNip25sThreeReadings() {
        fun content(reaction: Reaction) = event(react(row(1u), reaction)).content
        assertEquals("+", content(Reaction.Like))
        assertEquals("-", content(Reaction.Dislike))
        assertEquals("\uD83D\uDD25", content(Reaction.Emoji("\uD83D\uDD25")))
    }

    /** NIP-25 says there MUST always be an `e` tag set to the id of the event
     * being reacted to, so reacting to a reply names the REPLY -- a client
     * tallying by the first `e` cannot credit the thread root with a reaction
     * nobody gave it. */
    @Test
    fun reactingToAReplyNamesTheReplyAndNeverItsRoot() {
        val rootId = "f".repeat(64)
        val reply = row(1u, tags = listOf(listOf("e", rootId, "", "root")))
        val composed = event(react(reply, Reaction.Like))
        val eRows = composed.tags.filter { it[0] == "e" }
        assertEquals(1, eRows.size)
        assertEquals(reply.id, eRows[0][1])
    }

    /** Both refusals are typed and synchronous: an empty emoji is NIP-25's
     * spelling of a LIKE, and a NIP-30 `:shortcode:` needs a companion `emoji`
     * row this door does not write. */
    @Test
    fun anEmojiThatWouldSaySomethingElseRefuses() {
        for (emoji in listOf("", ":soapbox:")) {
            assertFailsWith<NMPError.InvalidReaction> {
                react(row(1u), Reaction.Emoji(emoji))
            }
        }
    }

    /** A malformed key is a typed refusal; nothing partial escapes. */
    @Test
    fun aMalformedNamedKeyRefuses() {
        assertFailsWith<NMPError.InvalidPublicKey> {
            chat().withContent(listOf(ContentPart.Person("not-a-key", null)))
        }
    }
}
