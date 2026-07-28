package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiReactionException

class NIP25Test {
    private val secret = "0".repeat(63) + "1"

    private suspend fun seedCanonicalTarget(engine: NMPEngine): String {
        val account = engine.addAccount(secret)
        engine.setActiveAccount(account.publicKey)
        val receipt =
            engine.publish(
                WriteIntent(
                    payload =
                        WritePayload.Unsigned(
                            pubkey = account.publicKey,
                            createdAt = 42uL,
                            kind = 1u,
                            tags = emptyList(),
                            content = "canonical target",
                        ),
                    durability = Durability.Durable,
                    routing = WriteRouting.AuthorOutbox,
                ),
            )
        val signed =
            withTimeout(5_000) {
                receipt.status.first { it is WriteStatus.Signed }
            } as WriteStatus.Signed
        val observed =
            withTimeout(5_000) {
                engine
                    .observe(
                        NMPFilter(
                            kinds = listOf(1u),
                            authors = NMPBinding.Literal(setOf(account.publicKey)),
                        ),
                    ).first { batch -> batch.rows.any { it.id == signed.eventId } }
            }
        assertEquals(signed.eventId, observed.rows.first { it.id == signed.eventId }.id)
        return signed.eventId
    }

    private fun fabricatedRow(id: String): Row =
        Row(
            id = id,
            pubkey = "f".repeat(64),
            createdAt = ULong.MAX_VALUE,
            kind = UShort.MAX_VALUE,
            tags =
                listOf(
                    listOf("h", "attacker-group"),
                    listOf("e", "a".repeat(64)),
                ),
            content = "native-forged body",
            sig = "0".repeat(128),
            sources = listOf("wss://attacker.invalid"),
        )

    @Test
    fun callerConstructibleRowCanOnlySelectCanonicalEventId() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val eventId = seedCanonicalTarget(engine)

                // Every field except id contradicts the canonical event.
                val target = engine.reactionTarget(fabricatedRow(eventId))
                engine.reactionDraft(target, ReactionValue.Like)
                engine.reactionDraft(
                    target,
                    ReactionValue.CustomEmoji(
                        shortcode = "soapbox",
                        imageUrl = "https://cdn.example/soapbox.png",
                    ),
                )
            }
            Unit
        }

    @Test
    fun malformedUnknownSignedOutAndInvalidValueAreTypedRefusals() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                var error =
                    assertThrows(ReactionError::class.java) {
                        engine.reactionTarget(fabricatedRow("bad"))
                    }
                assertEquals(ReactionError.InvalidEventId("bad"), error)

                val unknown = "7".repeat(64)
                error =
                    assertThrows(ReactionError::class.java) {
                        engine.reactionTarget(fabricatedRow(unknown))
                    }
                assertEquals(ReactionError.TargetNotFound(unknown), error)

                val eventId = seedCanonicalTarget(engine)
                val target = engine.reactionTarget(fabricatedRow(eventId))
                engine.setActiveAccount(null)
                error =
                    assertThrows(ReactionError::class.java) {
                        engine.reactionDraft(target, ReactionValue.Like)
                    }
                assertEquals(ReactionError.NoActiveReactionAuthor, error)

                val account = engine.addAccount(secret)
                engine.setActiveAccount(account.publicKey)
                error =
                    assertThrows(ReactionError::class.java) {
                        engine.reactionDraft(target, ReactionValue.Emoji(":missing:"))
                    }
                assertEquals(ReactionError.CustomEmojiRequiresMetadata(":missing:"), error)
            }
        }

    @Test
    fun everyFfiFailureKeepsItsTypedNativeAxis() {
        val cases =
            listOf(
                FfiReactionException.InvalidEventId("id") to ReactionError.InvalidEventId("id"),
                FfiReactionException.TargetNotFound("id") to ReactionError.TargetNotFound("id"),
                FfiReactionException.TargetNotVerified("id") to ReactionError.TargetNotVerified("id"),
                FfiReactionException.CanonicalLookupUnavailable("closed") to
                    ReactionError.CanonicalLookupUnavailable("closed"),
                FfiReactionException.EngineClosed() to ReactionError.EngineClosed,
                FfiReactionException.NoActiveReactionAuthor() to
                    ReactionError.NoActiveReactionAuthor,
                FfiReactionException.EmptyEmoji() to ReactionError.EmptyEmoji,
                FfiReactionException.StandardValueRequiresTypedVariant("+") to
                    ReactionError.StandardValueRequiresTypedVariant("+"),
                FfiReactionException.CustomEmojiRequiresMetadata(":x:") to
                    ReactionError.CustomEmojiRequiresMetadata(":x:"),
                FfiReactionException.InvalidEmojiToken("two words") to
                    ReactionError.InvalidEmojiToken("two words"),
                FfiReactionException.InvalidCustomEmojiShortcode("bad!") to
                    ReactionError.InvalidCustomEmojiShortcode("bad!"),
                FfiReactionException.InvalidCustomEmojiUrl("file:///x") to
                    ReactionError.InvalidCustomEmojiUrl("file:///x"),
            )
        cases.forEach { (ffi, expected) ->
            assertEquals(expected, ReactionError.from(ffi))
        }
    }
}
