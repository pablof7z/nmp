package com.nmp.sdk

import kotlinx.coroutines.async
import kotlinx.coroutines.coroutineScope
import kotlinx.coroutines.delay
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.fail
import org.junit.jupiter.api.Test

/**
 * #1284: the app-supplied signer mailbox as a Kotlin app actually holds it.
 *
 * The headline falsifier is cancellation. Every other pull handle here may be
 * closed when its collecting scope dies, because closing one ends one stream.
 * This mailbox IS the app's registered signer, so the same reflex would
 * silently park every later write for that key. Kotlin needs no explicit
 * bridge -- UniFFI's generated `suspendCancellableCoroutine` frees the parked
 * Rust future, releasing the single-reader claim -- but that is a property to
 * PROVE, not to assume, and it is the reason `requests()` has no
 * `finally { cancel() }`.
 */
class SignerMailboxTest {
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"
    private val appSecret = "0".repeat(63) + "1"

    /** A stand-in for the Keystore key / NIP-55 app / bunker an app really
     * reaches: a second engine that holds the secret and can sign the exact
     * requested body. */
    private suspend fun appSideSignature(
        body: NMPSignatureRequestBody,
        signer: NMPEngine,
    ): NMPSignedEvent =
        signer.signEvent(
            NMPUnsignedEvent(body.createdAt, body.kind, body.tags, body.content),
        )

    private suspend fun appSideSigner(): NMPEngine {
        val engine = NMPEngine(NMPConfig())
        engine.addAccount(appSecret)
        engine.setActiveAccount(author)
        return engine
    }

    /** What #1238 exists for, through the Kotlin surface. */
    @Test
    fun anAppSuppliedSignerSignsThroughItsMailbox(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                appSideSigner().use { appSigner ->
                    val mailbox = engine.addSigner(author)
                    assertEquals(author, mailbox.publicKey)
                    engine.setActiveAccount(author)

                    val signed =
                        async {
                            engine.signEvent(
                                NMPUnsignedEvent(1uL, 1u.toUShort(), emptyList(), "signed by the app"),
                            )
                        }
                    val request = requireNotNull(mailbox.next())
                    assertEquals(author, request.body.pubkey, "the author is frozen in the request")
                    assertEquals("signed by the app", request.body.content)
                    request.resolve(appSideSignature(request.body, appSigner))

                    assertEquals("signed by the app", signed.await().content)
                }
            }
        }

    /**
     * A collecting scope dies -- the ordinary case for a screen going away.
     * The collection must end, and the mailbox must still be the app's
     * signer afterwards. A `finally { cancel() }` copied from the row flow
     * would pass the first assertion and hang the second forever.
     */
    @Test
    fun cancellingACollectionEndsItAndLeavesTheSignerWorking(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                appSideSigner().use { appSigner ->
                    val mailbox = engine.addSigner(author)
                    engine.setActiveAccount(author)

                    // A collection parked on a mailbox with nothing in it.
                    coroutineScope {
                        val collecting = launch { mailbox.requests().collect { } }
                        delay(100) // let it reach the park before tearing it down
                        collecting.cancel()
                        withTimeout(5_000) { collecting.join() }
                        assertTrue(collecting.isCompleted, "a cancelled collection ends")
                    }

                    // The signer survived the collector that walked away.
                    val signed =
                        async {
                            engine.signEvent(
                                NMPUnsignedEvent(2uL, 1u.toUShort(), emptyList(), "the next scope signs"),
                            )
                        }
                    val request =
                        withTimeout(5_000) {
                            requireNotNull(mailbox.next()) {
                                "the mailbox outlived the collector, so it still delivers"
                            }
                        }
                    request.resolve(appSideSignature(request.body, appSigner))
                    assertEquals("the next scope signs", signed.await().content)
                }
            }
        }

    /**
     * The explicit non-destructive wake, which exists for Swift but is part
     * of this surface on both platforms: it ends one await and nothing else.
     */
    @Test
    fun unparkEndsOneAwaitAndNotTheMailbox(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                appSideSigner().use { appSigner ->
                    val mailbox = engine.addSigner(author)
                    engine.setActiveAccount(author)

                    mailbox.unpark()
                    assertNull(
                        withTimeout(5_000) { mailbox.next() },
                        "the armed unpark ends the await it was armed for",
                    )

                    val signed =
                        async {
                            engine.signEvent(
                                NMPUnsignedEvent(3uL, 1u.toUShort(), emptyList(), "one await, not the mailbox"),
                            )
                        }
                    val request = withTimeout(5_000) { requireNotNull(mailbox.next()) }
                    request.resolve(appSideSignature(request.body, appSigner))
                    assertEquals("one await, not the mailbox", signed.await().content)
                }
            }
        }

    /** `cancel()` is the destructive verb, and stays distinguishable. */
    @Test
    fun cancellingTheMailboxIsTheDestructiveOne(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val mailbox = engine.addSigner(author)
                mailbox.cancel()
                assertNull(withTimeout(5_000) { mailbox.next() })
                assertNull(withTimeout(5_000) { mailbox.next() }, "and it stays closed")
            }
        }

    /** A refusal is the app's own terminal answer, not a timeout. */
    @Test
    fun anAppRefusalReachesTheCaller(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val mailbox = engine.addSigner(author)
                engine.setActiveAccount(author)

                // The app answers from its own coroutine, as it would from its
                // own signer; the refusal must reach the caller of signEvent.
                val responder =
                    launch {
                        requireNotNull(mailbox.next())
                            .reject(NMPSignerRejection.Rejected("user declined"))
                    }
                try {
                    engine.signEvent(
                        NMPUnsignedEvent(4uL, 1u.toUShort(), emptyList(), "the user declines"),
                    )
                    fail<Unit>("a declined signature must not succeed")
                } catch (failure: NMPError.SignerRejected) {
                    assertEquals("user declined", failure.reason)
                }
                responder.join()
            }
        }

    /** Each request carries exactly one answer. */
    @Test
    fun aRequestSettlesExactlyOnce(): Unit =
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                appSideSigner().use { appSigner ->
                    val mailbox = engine.addSigner(author)
                    engine.setActiveAccount(author)
                    val signed =
                        async {
                            engine.signEvent(
                                NMPUnsignedEvent(5uL, 1u.toUShort(), emptyList(), "settled once"),
                            )
                        }

                    val request = requireNotNull(mailbox.next())
                    request.resolve(appSideSignature(request.body, appSigner))
                    assertEquals("settled once", signed.await().content)

                    assertThrows(NMPSignatureSettleError.AlreadySettled::class.java) {
                        request.reject(NMPSignerRejection.Unavailable)
                    }
                }
            }
        }

    /** Removal is exact-instance: a superseded mailbox detaches nothing. */
    @Test
    fun removalIsStaleSafe() {
        NMPEngine(NMPConfig()).use { engine ->
            val first = engine.addSigner(author)
            val replacement = engine.addSigner(author)
            assertEquals(false, engine.removeSigner(first), "a stale mailbox detaches nothing")
            assertEquals(true, engine.removeSigner(replacement))
            assertEquals(false, engine.removeSigner(replacement))
        }
    }
}
