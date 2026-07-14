package com.nmp.sdk

import java.io.ByteArrayOutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.charset.StandardCharsets
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicLong
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread
import kotlinx.coroutines.CoroutineStart
import kotlinx.coroutines.async
import kotlinx.coroutines.runBlocking
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.fail
import org.junit.jupiter.api.Test

private class LocalNIP11Server(
    private val body: String,
    private val responseDelayMillis: Long = 0,
) : AutoCloseable {
    private val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
    private val accepted = CountDownLatch(1)
    private val responseTime = AtomicLong(0)
    private val failure = AtomicReference<Throwable?>()
    private val worker =
        thread(name = "nmp-kotlin-nip11-fixture", isDaemon = true) {
            try {
                server.accept().use { socket ->
                    val input = socket.getInputStream()
                    val request = ByteArrayOutputStream()
                    var matched = 0
                    val terminator = byteArrayOf(13, 10, 13, 10)
                    while (matched < terminator.size) {
                        val byte = input.read()
                        check(byte >= 0) { "NIP-11 request ended before its headers" }
                        request.write(byte)
                        matched = if (byte.toByte() == terminator[matched]) matched + 1 else 0
                    }
                    check(
                        request.toString(StandardCharsets.US_ASCII)
                            .contains("Accept: application/nostr+json", ignoreCase = true),
                    ) { "NIP-11 request omitted its media-type accept header" }
                    accepted.countDown()
                    Thread.sleep(responseDelayMillis)
                    val bytes = body.toByteArray(StandardCharsets.UTF_8)
                    val headers =
                        ("HTTP/1.1 200 OK\r\n" +
                            "Content-Type: application/nostr+json\r\n" +
                            "Content-Length: ${bytes.size}\r\n" +
                            "Connection: close\r\n\r\n")
                            .toByteArray(StandardCharsets.US_ASCII)
                    responseTime.set(System.nanoTime())
                    socket.getOutputStream().apply {
                        write(headers)
                        write(bytes)
                        flush()
                    }
                }
            } catch (error: Throwable) {
                if (!server.isClosed) failure.set(error)
                accepted.countDown()
            }
        }

    val relayUrl: String = "ws://localhost:${server.localPort}"

    fun awaitAccepted(): Boolean {
        val result = accepted.await(2, TimeUnit.SECONDS)
        failure.get()?.let { throw AssertionError("local NIP-11 fixture failed", it) }
        return result
    }

    fun respondedAt(): Long = responseTime.get()

    override fun close() {
        server.close()
        worker.join(2_000)
        failure.get()?.let { throw AssertionError("local NIP-11 fixture failed", it) }
    }
}

class RelayInformationTest {
    @Test
    fun publicSuspendCallYieldsCallerThreadAndDeliversSuccess() =
        runBlocking {
            LocalNIP11Server(
                body = """{"name":"Local","supported_nips":[11,77],"limitation":{"max_limit":500,"auth_required":true}}""",
                responseDelayMillis = 500,
            ).use { server ->
                NMPEngine(NMPConfig()).use { engine ->
                    val request =
                        async(start = CoroutineStart.UNDISPATCHED) {
                            engine.relayInformation(
                                server.relayUrl,
                                RelayInformationCachePolicy.Refresh,
                            )
                        }
                    assertTrue(server.awaitAccepted(), "the generated suspend call must start HTTP")
                    val callerProgress = System.nanoTime()
                    val value = request.await()

                    assertEquals("Local", value.document.name)
                    assertEquals(listOf(11.toUShort(), 77.toUShort()), value.document.supportedNips)
                    assertEquals(64, value.documentRevision.length)
                    assertEquals(500.toULong(), value.document.limitation.maxLimit)
                    assertEquals(true, value.document.limitation.authRequired)
                    assertTrue(server.respondedAt() > 0, "fixture never sent its delayed response")
                    assertTrue(
                        callerProgress < server.respondedAt(),
                        "the caller coroutine must resume while Rust is still waiting for HTTP",
                    )
                }
            }
        }

    @Test
    fun publicSuspendCallDeliversTypedAcquisitionError() =
        runBlocking {
            LocalNIP11Server(body = "not-json").use { server ->
                NMPEngine(NMPConfig()).use { engine ->
                    try {
                        engine.relayInformation(
                            server.relayUrl,
                            RelayInformationCachePolicy.Refresh,
                        )
                        fail("malformed NIP-11 must fail without an invented empty document")
                    } catch (error: NMPError.RelayInformationUnavailable) {
                        assertTrue(error.reason.isNotEmpty())
                    }
                }
            }
        }
}
