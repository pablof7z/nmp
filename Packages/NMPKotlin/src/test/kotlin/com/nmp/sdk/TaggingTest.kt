// #1243: the tagging door at the native boundary. Mirrors TaggingTests.swift.

package com.nmp.sdk

import kotlin.test.Test
import kotlin.test.assertEquals
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
        sig = "c".repeat(128),
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
        val filled = event(draft.withContent("hello"))
        assertEquals("", bare.content)
        assertEquals("hello", filled.content)
        assertEquals(bare.tags, filled.tags)
    }
}
