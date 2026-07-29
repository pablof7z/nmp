package com.nmp.sdk

import java.io.ByteArrayOutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.Base64
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread
import kotlinx.coroutines.CancellationException
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.int
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.jupiter.api.Assertions.assertArrayEquals
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.fail
import org.junit.jupiter.api.Test

/** A real local TCP server that speaks just enough of BUD-02 `PUT /upload` to
 * answer the engine-authorized upload: it reads the complete request (headers
 * plus `Content-Length` body), records exactly what it received, and replies
 * with a descriptor computed from THOSE bytes.
 *
 * Deliberately not a mock of the Kotlin wrapper: the falsifiers below assert
 * on what actually crossed the socket, which is the only place a claim about
 * "the exact bytes" and "NMP owns the BUD-11 header" can be checked. */
private class LocalBlossomServer(
    gated: Boolean = false,
) : AutoCloseable {
    private val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
    private val received = CountDownLatch(1)
    private val responseGate = CountDownLatch(if (gated) 1 else 0)
    private val failure = AtomicReference<Throwable?>()
    private val head = AtomicReference("")
    private val body = AtomicReference(ByteArray(0))

    private val worker =
        thread(name = "nmp-kotlin-blossom-fixture", isDaemon = true) {
            try {
                server.accept().use { socket ->
                    val input = socket.getInputStream()
                    val request = ByteArrayOutputStream()
                    var matched = 0
                    val terminator = byteArrayOf(13, 10, 13, 10)
                    while (matched < terminator.size) {
                        val byte = input.read()
                        check(byte >= 0) { "upload request ended before its headers" }
                        request.write(byte)
                        matched = if (byte.toByte() == terminator[matched]) matched + 1 else 0
                    }
                    val headText = request.toString(StandardCharsets.ISO_8859_1)
                    head.set(headText)
                    val contentLength =
                        headText.lineSequence()
                            .mapNotNull { line ->
                                val separator = line.indexOf(':')
                                if (separator < 0) {
                                    null
                                } else if (!line.substring(0, separator)
                                        .equals("content-length", ignoreCase = true)
                                ) {
                                    null
                                } else {
                                    line.substring(separator + 1).trim().toIntOrNull()
                                }
                            }
                            .firstOrNull() ?: 0
                    val uploaded = ByteArray(contentLength)
                    var read = 0
                    while (read < contentLength) {
                        val count = input.read(uploaded, read, contentLength - read)
                        check(count > 0) { "upload request ended before its body" }
                        read += count
                    }
                    body.set(uploaded)
                    received.countDown()
                    check(responseGate.await(5, TimeUnit.SECONDS)) {
                        "response gate was not released"
                    }
                    val digest =
                        MessageDigest.getInstance("SHA-256").digest(uploaded)
                            .joinToString("") { "%02x".format(it) }
                    val descriptor =
                        (
                            """{"url":"https://cdn.example/$digest","sha256":"$digest",""" +
                                """"size":$contentLength,"type":"application/pdf"}"""
                        ).toByteArray(StandardCharsets.UTF_8)
                    val headers =
                        (
                            "HTTP/1.1 201 Created\r\n" +
                                "Content-Type: application/json\r\n" +
                                "Content-Length: ${descriptor.size}\r\n" +
                                "Connection: close\r\n\r\n"
                        ).toByteArray(StandardCharsets.US_ASCII)
                    socket.getOutputStream().apply {
                        write(headers)
                        write(descriptor)
                        flush()
                    }
                }
            } catch (error: Throwable) {
                if (!server.isClosed) failure.set(error)
                received.countDown()
            }
        }

    val serverUrl: String = "http://127.0.0.1:${server.localPort}"

    fun awaitRequest(timeoutSeconds: Long = 5): Boolean {
        val result = received.await(timeoutSeconds, TimeUnit.SECONDS)
        failure.get()?.let { throw AssertionError("local Blossom fixture failed", it) }
        return result
    }

    fun releaseResponse() = responseGate.countDown()

    fun capturedHead(): String = head.get()

    fun capturedBody(): ByteArray = body.get()

    fun headerValue(name: String): String? =
        capturedHead().lineSequence()
            .mapNotNull { line ->
                val separator = line.indexOf(':')
                if (separator < 0 ||
                    !line.substring(0, separator).equals(name, ignoreCase = true)
                ) {
                    null
                } else {
                    line.substring(separator + 1).trim()
                }
            }
            .firstOrNull()

    override fun close() {
        responseGate.countDown()
        server.close()
        worker.join(2_000)
    }
}

class BlossomUploadTest {
    private val secret = "0".repeat(63) + "1"
    private val author = "79be667ef9dcbbac55a06295ce870b07029bfcdb2dce28d959f2815b16f81798"

    private fun config() = NMPConfig(allowedLocalRelayHosts = listOf("127.0.0.1", "localhost"))

    private fun sha256Hex(bytes: ByteArray): String =
        MessageDigest.getInstance("SHA-256").digest(bytes).joinToString("") { "%02x".format(it) }

    /** #971's headline Kotlin falsifier: the app states four product inputs,
     * and the EXACT bytes it named reach the wire under a BUD-11 header the
     * app never saw, signed by the active account. */
    @Test
    fun uploadBlossomSendsExactBytesUnderAnNmpOwnedBud11Header() =
        runBlocking {
            LocalBlossomServer().use { server ->
                NMPEngine(config()).use { engine ->
                    engine.addAccount(secret)
                    engine.setActiveAccount(author)
                    val blob =
                        "%PDF exact kotlin bytes\r\n".toByteArray(StandardCharsets.UTF_8) +
                            byteArrayOf(0x00, 0xff.toByte(), 0x7f, 0x80.toByte())

                    val descriptor =
                        engine.uploadBlossom(
                            serverUrl = server.serverUrl,
                            blob = blob,
                            contentType = "application/pdf",
                            description = "Upload the signed report",
                        )

                    assertTrue(server.awaitRequest())
                    assertArrayEquals(blob, server.capturedBody())
                    assertTrue(server.capturedHead().startsWith("PUT /upload HTTP/1.1\r\n"))
                    assertEquals("application/pdf", server.headerValue("content-type"))

                    val expected = sha256Hex(blob)
                    assertEquals(expected, server.headerValue("x-sha-256"))
                    assertEquals(expected, descriptor.sha256)
                    assertEquals(blob.size.toULong(), descriptor.size)

                    val header = server.headerValue("authorization") ?: fail("missing BUD-11 header")
                    val encoded = header.substringAfterLast(' ')
                    val event =
                        Json.parseToJsonElement(
                            String(
                                Base64.getUrlDecoder().decode(encoded),
                                StandardCharsets.UTF_8,
                            ),
                        ).jsonObject
                    assertEquals(
                        author,
                        event.getValue("pubkey").jsonPrimitive.content,
                    )
                    assertEquals(24_242, event.getValue("kind").jsonPrimitive.int)
                    val flattened =
                        event.getValue("tags").jsonArray.map { tag ->
                            tag.jsonArray.map { it.jsonPrimitive.content }
                        }
                    assertTrue(flattened.contains(listOf("t", "upload")))
                    assertTrue(flattened.contains(listOf("x", expected)))
                    assertTrue(flattened.any { it.firstOrNull() == "expiration" })
                }
            }
        }

    /** A signer failure is typed and reaches the network zero times. */
    @Test
    fun uploadBlossomWithoutAnActiveSignerIsTypedAndMakesNoRequest() =
        runBlocking {
            LocalBlossomServer().use { server ->
                NMPEngine(config()).use { engine ->
                    engine.setActiveAccount(author)
                    try {
                        engine.uploadBlossom(
                            serverUrl = server.serverUrl,
                            blob = "no signer".toByteArray(StandardCharsets.UTF_8),
                            contentType = "application/pdf",
                            description = "refused",
                        )
                        fail("an upload with no signer must fail")
                    } catch (failure: BlossomUploadFailure) {
                        assertTrue(failure is BlossomUploadFailure.NoActiveSigner)
                    }
                    assertTrue(
                        !server.awaitRequest(timeoutSeconds = 1),
                        "a signer refusal must not reach the server",
                    )
                }
            }
        }

    /** Every refusal keeps its own case rather than becoming a string. */
    @Test
    fun uploadBlossomPreflightRefusalsAreTyped() =
        runBlocking {
            NMPEngine(config()).use { engine ->
                engine.addAccount(secret)
                engine.setActiveAccount(author)
                try {
                    engine.uploadBlossom(
                        serverUrl = "ftp://blobs.example",
                        blob = "scheme".toByteArray(StandardCharsets.UTF_8),
                        contentType = "application/pdf",
                        description = "refused",
                    )
                    fail("a non-http server URL must fail")
                } catch (failure: BlossomUploadFailure) {
                    assertEquals(
                        BlossomUploadFailure.InvalidServerUrl(
                            BlossomServerUrlError.UnsupportedScheme("ftp"),
                        ),
                        failure,
                    )
                }
                try {
                    engine.uploadBlossom(
                        serverUrl = "https://blobs.example",
                        blob = "empty".toByteArray(StandardCharsets.UTF_8),
                        contentType = "",
                        description = "refused",
                    )
                    fail("an empty content type must fail")
                } catch (failure: BlossomUploadFailure) {
                    assertTrue(failure is BlossomUploadFailure.EmptyContentType)
                }
            }
        }

    /** Coroutine cancellation reaches Rust: the awaiting coroutine is
     * cancelled while the gated server holds the response, and the wrapper
     * reports cancellation rather than inventing a success or a Blossom
     * fault. */
    @Test
    fun cancellingTheAwaitingCoroutineWithdrawsTheUpload() =
        runBlocking {
            LocalBlossomServer(gated = true).use { server ->
                NMPEngine(config()).use { engine ->
                    engine.addAccount(secret)
                    engine.setActiveAccount(author)
                    val blob = "cancel during HTTP".toByteArray(StandardCharsets.UTF_8)
                    // `Dispatchers.IO`, not `runBlocking`'s own event loop:
                    // the assertion below parks this thread on a latch, and a
                    // child on the same single-threaded loop would never get
                    // to start.
                    val upload =
                        async(Dispatchers.IO) {
                            engine.uploadBlossom(
                                serverUrl = server.serverUrl,
                                blob = blob,
                                contentType = "application/pdf",
                                description = "withdrawn",
                            )
                        }
                    assertTrue(server.awaitRequest(), "the request must reach the gated server")
                    assertArrayEquals(blob, server.capturedBody())
                    upload.cancel()
                    try {
                        upload.await()
                        fail("a cancelled upload must not report success")
                    } catch (_: CancellationException) {
                        // The observation gap: the bytes were transmitted, the
                        // local operation stopped, and nothing claims what the
                        // remote did with them.
                    }
                    server.releaseResponse()
                }
            }
        }
}
