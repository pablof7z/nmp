package com.nmp.sdk

import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotEquals
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Test

/** FALSIFIER (author binding): the typed refusal survives to Kotlin. A
 * BUD-11 draft composed for account A, signed by the engine while account
 * B is current, is refused as [BlossomAuthError.AuthorMismatch] -- the
 * signature is genuinely valid for B, so [BlossomAuthError.BadSignature]
 * cannot fire and every other BUD-11 check passes. Without this the SDK
 * hands back an authorization acting as B while the caller believes it
 * speaks for A. */
class BlossomAuthorBindingTest {
    private val declaredSecret = "0".repeat(63) + "1"
    private val signingSecret = "0".repeat(63) + "2"

    private fun ByteArray.hex(): String =
        joinToString(separator = "") { byte -> "%02x".format(byte.toInt() and 0xff) }

    @Test
    fun engineSignedAuthorizationUnderADifferentAccountIsRefused() =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val declared = engine.session.add(declaredSecret.testPrivateKey())
                val signing = engine.session.add(signingSecret.testPrivateKey(), makeCurrent = true)
                val declaredHex = declared.publicKey.bytes.hex()
                val signingHex = signing.publicKey.bytes.hex()
                assertNotEquals(declaredHex, signingHex)

                val blobHex = "b".repeat(64)
                val now = 1_700_000_000uL
                val draft =
                    blossomUploadAuthorizationDraft(
                        authorPubkeyHex = declaredHex,
                        blobSha256Hex = blobHex,
                        createdAt = now - 5uL,
                        expiration = now + 600uL,
                        description = "upload as the declared account",
                    )
                assertEquals(declaredHex, draft.authorPubkeyHex)

                // `signEvent` freezes the author from the CURRENT account,
                // which here is not the one the draft was composed for.
                val signed = engine.signEvent(draft.signRequest)
                assertEquals(signingHex, signed.pubkey)

                val error =
                    assertThrows(BlossomAuthError.AuthorMismatch::class.java) {
                        BlossomAuthorization.validate(
                            signedEvent = signed,
                            authorPubkeyHex = draft.authorPubkeyHex,
                            verb = BlossomVerb.UPLOAD,
                            blobSha256Hex = blobHex,
                            now = now,
                        )
                    }
                assertEquals(declaredHex, error.expectedPubkeyHex)
                assertEquals(signingHex, error.foundPubkeyHex)

                // The refusal is about identity, not a blanket rejection:
                // the same signed event validates under the account that
                // actually signed it.
                val auth =
                    BlossomAuthorization.validate(
                        signedEvent = signed,
                        authorPubkeyHex = signingHex,
                        verb = BlossomVerb.UPLOAD,
                        blobSha256Hex = blobHex,
                        now = now,
                    )
                assertEquals(BlossomVerb.UPLOAD, auth.verb)
                assertEquals(blobHex, auth.blobSha256Hex)
            }
        }
}
