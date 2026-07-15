package com.nmp.sdk

import kotlinx.coroutines.async
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertNotNull
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assertions.assertThrows
import org.junit.jupiter.api.Assertions.fail
import org.junit.jupiter.api.Test
import java.io.ByteArrayOutputStream
import java.io.EOFException
import java.io.InputStream
import java.io.OutputStream
import java.net.InetAddress
import java.net.ServerSocket
import java.net.Socket
import java.net.SocketException
import java.nio.charset.StandardCharsets
import java.security.MessageDigest
import java.util.Base64
import java.util.Collections
import java.util.concurrent.CopyOnWriteArrayList
import java.util.concurrent.LinkedBlockingQueue
import java.util.concurrent.TimeUnit
import java.util.concurrent.atomic.AtomicBoolean
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread

/** A real local WebSocket relay for the generated-native ACK falsifier.
 * It performs the RFC 6455 handshake, reads the masked EVENT frame emitted
 * by NMP's production transport, and returns a real Nostr OK frame. */
private class LocalAckRelay : AutoCloseable {
    private val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
    private val accepted = Collections.synchronizedList(mutableListOf<Socket>())
    private val connectionWorkers = Collections.synchronizedList(mutableListOf<Thread>())
    private val closed = AtomicBoolean(false)
    private val clientClosed = AtomicBoolean(false)
    private val failure = AtomicReference<Throwable?>()
    private val events = LinkedBlockingQueue<String>()
    private val storedEvents = CopyOnWriteArrayList<String>()
    val url: String = "ws://127.0.0.1:${server.localPort}"

    private val worker =
        thread(name = "nmp-kotlin-local-ack-relay", isDaemon = true) {
            try {
                while (!closed.get()) {
                    val socket = server.accept()
                    accepted.add(socket)
                    val connection =
                        thread(name = "nmp-kotlin-local-relay-connection", isDaemon = true) {
                            serve(socket)
                        }
                    connectionWorkers.add(connection)
                }
            } catch (error: Throwable) {
                if (!closed.get()) failure.set(error)
            }
        }

    private fun serve(socket: Socket) {
        try {
            socket.tcpNoDelay = true
            val input = socket.getInputStream()
            val output = socket.getOutputStream()
            if (!completeHandshake(input, output)) return
            while (!socket.isClosed) {
                val frame = readFrame(input) ?: break
                when (frame.opcode) {
                    0x1 -> {
                        val text = frame.payload.toString(StandardCharsets.UTF_8)
                        when {
                            text.startsWith("[\"EVENT\",") -> {
                                val id = eventId(text)
                                storedEvents.add(text.substringAfter(',').dropLast(1))
                                events.put(text)
                                writeText(output, "[\"OK\",\"$id\",true,\"saved\"]")
                            }
                            text.startsWith("[\"REQ\",") -> {
                                val subscriptionId =
                                    Regex("^\\[\\s*\"REQ\"\\s*,\\s*\"([^\"]+)\"")
                                        .find(text)
                                        ?.groupValues
                                        ?.get(1)
                                        ?: error("REQ frame had no subscription id: $text")
                                for (event in storedEvents) {
                                    writeText(output, "[\"EVENT\",\"$subscriptionId\",$event]")
                                }
                                writeText(output, "[\"EOSE\",\"$subscriptionId\"]")
                            }
                        }
                    }
                    0x8 -> {
                        clientClosed.set(true)
                        break
                    }
                    0x9 -> writeFrame(output, 0xA, frame.payload)
                }
            }
        } catch (error: SocketException) {
            // Fixture-initiated teardown (close()) or a client-initiated close
            // frame (opcode 0x8) already observed means a subsequent
            // SocketException on this connection is expected teardown noise,
            // not a real relay failure.
            if (!closed.get() && !clientClosed.get()) failure.compareAndSet(null, error)
        } catch (error: Throwable) {
            if (!closed.get()) failure.compareAndSet(null, error)
        }
    }

    fun awaitEvent(): String {
        val event = events.poll(10, TimeUnit.SECONDS)
        failure.get()?.let { throw AssertionError("local relay failed", it) }
        return event ?: throw AssertionError("local relay never received an EVENT frame")
    }

    override fun close() {
        closed.set(true)
        server.close()
        synchronized(accepted) { accepted.forEach(Socket::close) }
        worker.join(2_000)
        synchronized(connectionWorkers) { connectionWorkers.forEach { it.join(2_000) } }
        failure.get()?.let { throw AssertionError("local relay failed", it) }
    }

    private fun eventId(frame: String): String =
        Regex("\\\"id\\\"\\s*:\\s*\\\"([0-9a-f]{64})\\\"")
            .find(frame)
            ?.groupValues
            ?.get(1)
            ?: error("EVENT frame had no canonical id: $frame")

    private fun writeText(
        output: OutputStream,
        text: String,
    ) {
        writeFrame(output, 0x1, text.toByteArray(StandardCharsets.UTF_8))
    }

    /** Serve NIP-11 HTTP on the same origin as the WebSocket test relay, or
     * complete the RFC 6455 upgrade. Production opens the NIP-11 request
     * independently, so the relay must not misclassify it as a broken
     * WebSocket handshake. */
    private fun completeHandshake(
        input: InputStream,
        output: OutputStream,
    ): Boolean {
        val bytes = ByteArrayOutputStream()
        var matched = 0
        val terminator = byteArrayOf(13, 10, 13, 10)
        while (matched < terminator.size) {
            val next = input.read()
            if (next < 0) throw EOFException("client closed during WebSocket handshake")
            bytes.write(next)
            matched = if (next.toByte() == terminator[matched]) matched + 1 else 0
            check(bytes.size() <= 16_384) { "oversized WebSocket handshake" }
        }
        val request = bytes.toString(StandardCharsets.US_ASCII)
        val key =
            request
                .lineSequence()
                .firstOrNull { it.startsWith("Sec-WebSocket-Key:", ignoreCase = true) }
                ?.substringAfter(':')
                ?.trim()
        if (key == null) {
            check(request.startsWith("GET ") && request.contains("application/nostr+json")) {
                "request was neither NIP-11 HTTP nor a WebSocket upgrade"
            }
            val body = "{\"supported_nips\":[29]}"
            output.write(
                ("HTTP/1.1 200 OK\r\n" +
                    "Content-Type: application/nostr+json\r\n" +
                    "Content-Length: ${body.toByteArray().size}\r\n" +
                    "Connection: close\r\n\r\n" +
                    body)
                    .toByteArray(StandardCharsets.US_ASCII),
            )
            output.flush()
            return false
        }
        val accept =
            Base64.getEncoder().encodeToString(
                MessageDigest.getInstance("SHA-1")
                    .digest((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").toByteArray()),
            )
        output.write(
            ("HTTP/1.1 101 Switching Protocols\r\n" +
                "Upgrade: websocket\r\n" +
                "Connection: Upgrade\r\n" +
                "Sec-WebSocket-Accept: $accept\r\n\r\n")
                .toByteArray(StandardCharsets.US_ASCII),
        )
        output.flush()
        return true
    }

    private data class Frame(
        val opcode: Int,
        val payload: ByteArray,
    )

    private fun readFrame(input: InputStream): Frame? {
        val first = input.read()
        if (first < 0) return null
        val second = input.read()
        if (second < 0) throw EOFException("truncated WebSocket frame")
        var length = (second and 0x7f).toLong()
        if (length == 126L) {
            length = ((input.readRequired() shl 8) or input.readRequired()).toLong()
        } else if (length == 127L) {
            length = 0
            repeat(8) { length = (length shl 8) or input.readRequired().toLong() }
        }
        check(length <= Int.MAX_VALUE) { "oversized WebSocket frame" }
        val mask = if ((second and 0x80) != 0) input.readExactly(4) else null
        val payload = input.readExactly(length.toInt())
        if (mask != null) {
            for (index in payload.indices) {
                payload[index] = (payload[index].toInt() xor mask[index % 4].toInt()).toByte()
            }
        }
        return Frame(first and 0x0f, payload)
    }

    private fun writeFrame(
        output: OutputStream,
        opcode: Int,
        payload: ByteArray,
    ) {
        output.write(0x80 or opcode)
        when {
            payload.size < 126 -> output.write(payload.size)
            payload.size <= 0xffff -> {
                output.write(126)
                output.write(payload.size shr 8)
                output.write(payload.size)
            }
            else -> {
                output.write(127)
                repeat(4) { output.write(0) }
                output.write(payload.size shr 24)
                output.write(payload.size shr 16)
                output.write(payload.size shr 8)
                output.write(payload.size)
            }
        }
        output.write(payload)
        output.flush()
    }

    private fun InputStream.readRequired(): Int {
        val value = read()
        if (value < 0) throw EOFException("truncated WebSocket frame")
        return value
    }

    private fun InputStream.readExactly(size: Int): ByteArray {
        val bytes = ByteArray(size)
        var offset = 0
        while (offset < size) {
            val read = read(bytes, offset, size - offset)
            if (read < 0) throw EOFException("truncated WebSocket frame payload")
            offset += read
        }
        return bytes
    }
}

// The read-only NIP-29 host-browser projection (#108) -- construction/
// mapping tests only. No network: these are pure Demand constructors and
// a pure decode.
class NIP29Test {
    @Test
    fun activeAccountDemandTargetsKind10009() {
        val demand = activeAccountDemand()
        assertEquals(listOf<UShort>(10009u), demand.selection.kinds)
    }

    @Test
    fun groupDiscoveryDemandPinsTheParsedHost() {
        val demand = groupDiscoveryDemand("wss://host-1.example.com")
        assertEquals(listOf<UShort>(39000u), demand.selection.kinds)
        val source = demand.source as NMPSourceAuthority.Pinned
        assertEquals(setOf("wss://host-1.example.com"), source.relays)
    }

    @Test
    fun groupDiscoveryDemandRejectsAnUnparseableHost() {
        val error =
            assertThrows(NMPError.InvalidRelayUrl::class.java) {
                groupDiscoveryDemand("not-a-url")
            }
        assertEquals("not-a-url", error.got)
    }

    @Test
    fun groupContentDemandScopesByHTag() {
        val demand = groupContentDemand("wss://host-1.example.com", "group-a")
        assertEquals(listOf<UShort>(9u, 30315u), demand.selection.kinds)
    }

    @Test
    fun decodeRememberedGroupsComposesAKind10009Row() {
        val row =
            Row(
                id = "id",
                pubkey = "pubkey",
                createdAt = 1uL,
                kind = 10009u,
                tags = listOf(listOf("group", "group-a", "wss://relay-a.example.com", "Group A")),
                content = "",
                sig = "sig",
                sources = emptyList(),
            )
        val remembered = decodeRememberedGroups(row)
        assertEquals(1, remembered.groups.size)
        assertEquals("group-a", remembered.groups[0].groupId)
        assertEquals("wss://relay-a.example.com", remembered.groups[0].host)
        assertEquals("Group A", remembered.groups[0].name)
        assertFalse(remembered.hasPrivateContent)
    }

    // groupMessageIntent / publishComposed (#156)

    @Test
    fun groupMessageIntentRequiresAnActiveAccount() {
        NMPEngine(NMPConfig()).use { engine ->
            assertThrows(NMPError.NoActiveAccount::class.java) {
                engine.groupMessageIntent(
                    host = "wss://group-host.example.com",
                    groupId = "group-a",
                    content = "hello",
                )
            }
        }
    }

    @Test
    fun groupMessageIntentMaterializesTheCanonicalSemanticTemplate() {
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val author = engine.addAccount("0".repeat(63) + "1")
                engine.setActiveAccount(author)
                val host = "wss://group-host.example.com"
                val groupId = "group-a"
                val first = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
                val second = "7e7e9c42a91bfef19fa929e5fda1b72e0ebc1a4c1141673e2794234d86addf4e"
                val parentId = "1".repeat(64)
                val rowDeferred =
                    async {
                        withTimeoutOrNull(5_000) {
                            engine.observe(groupContentDemand(host, groupId))
                                .first { it.rows.isNotEmpty() }
                                .rows
                                .first()
                        }
                    }
                val intent =
                    engine.groupMessageIntent(
                        host = host,
                        groupId = groupId,
                        content = "hello",
                        recipients = listOf(first, first, second),
                        reply = GroupReplyParent(parentId, first),
                    )
                val receipt = engine.publishComposed(intent)
                assertEquals(WriteStatus.Accepted, withTimeoutOrNull(5_000) { receipt.status.first() })

                val row = rowDeferred.await()
                assertNotNull(row)
                assertEquals(author, row?.pubkey)
                assertEquals(9u.toUShort(), row?.kind)
                assertTrue((row?.createdAt ?: 0uL) > 1_700_000_000uL)
                assertEquals(
                    "nostr:npub180cvv07tjdrrgpa0j7j7tmnyl2yr6yr7l8j4s3evf6u64th6gkwsyjh6w6 " +
                        "nostr:npub10elfcs4fr0l0r8af98jlmgdh9c8tcxjvz9qkw038js35mp4dma8qzvjptg hello",
                    row?.content,
                )
                assertEquals(
                    listOf(
                        listOf("p", first),
                        listOf("p", second),
                        listOf("e", parentId, "", "reply", first),
                        listOf("h", groupId),
                    ),
                    row?.tags,
                )
            }
        }
    }

    @Test
    fun generatedNativeGroupMessageReachesRelayAckAndCanonicalReadBack() {
        LocalAckRelay().use { relay ->
            runBlocking {
                NMPEngine(NMPConfig()).use { engine ->
                    val author = engine.addAccount("0".repeat(63) + "1")
                    engine.setActiveAccount(author)
                    val groupId = "group-acked"
                    val receipt =
                        engine.publishComposed(
                            engine.groupMessageIntent(
                                host = relay.url,
                                groupId = groupId,
                                content = "generated native ack",
                            ),
                        )

                    val acked =
                        withTimeoutOrNull(10_000) {
                            receipt.status.first { it is WriteStatus.Acked }
                        } as? WriteStatus.Acked
                    assertNotNull(acked, "generated Kotlin API must observe a real relay Acked fact")
                    assertTrue(acked!!.relay.startsWith(relay.url))
                    val wireEvent = relay.awaitEvent()
                    assertTrue(wireEvent.startsWith("[\"EVENT\","))
                    assertTrue(wireEvent.contains("\"content\":\"generated native ack\""))

                    val row =
                        withTimeoutOrNull(10_000) {
                            engine.observe(groupContentDemand(relay.url, groupId))
                                .first { batch ->
                                    batch.rows.any { it.content == "generated native ack" }
                                }
                                .rows
                                .first { it.content == "generated native ack" }
                        }
                    assertNotNull(row, "the Acked event must read back through canonical NMP query state")
                    assertEquals(author, row?.pubkey)
                    assertEquals(9u.toUShort(), row?.kind)
                    assertTrue(row?.tags?.contains(listOf("h", groupId)) == true)
                }
            }
        }
    }

    @Test
    fun engineOwnedPreviousAdmitsOnlyTheSelectedHostsDeliveredRows() {
        LocalAckRelay().use { relayX ->
            LocalAckRelay().use { relayY ->
                runBlocking {
                    NMPEngine(NMPConfig()).use { engine ->
                        val author = engine.addAccount("0".repeat(63) + "1")
                        engine.setActiveAccount(author)
                        val groupId = "same-id-on-two-hosts"

                        suspend fun seed(relay: LocalAckRelay, content: String): String {
                            val receipt =
                                engine.publishComposed(
                                    engine.groupMessageIntent(
                                        host = relay.url,
                                        groupId = groupId,
                                        content = content,
                                    ),
                                )
                            val acked =
                                withTimeoutOrNull(10_000) {
                                    receipt.status.first { it is WriteStatus.Acked }
                                }
                            assertTrue(acked is WriteStatus.Acked)
                            val wire = relay.awaitEvent()
                            val id =
                                Regex("\\\"id\\\"\\s*:\\s*\\\"([0-9a-f]{64})\\\"")
                                    .find(wire)!!
                                    .groupValues[1]
                            val delivered =
                                withTimeoutOrNull(10_000) {
                                    engine.observe(groupContentDemand(relay.url, groupId))
                                        .first { batch ->
                                            batch.rows.any { row ->
                                                row.id == id &&
                                                    row.sources.any { it.startsWith(relay.url) }
                                            }
                                        }
                                }
                            assertNotNull(delivered, "$content must re-enter NMP from its relay")
                            return id
                        }

                        val xId = seed(relayX, "seen only on X")
                        val yId = seed(relayY, "seen only on Y")
                        assertFalse(xId.startsWith(yId.take(8)))

                        val receipt =
                            engine.publishComposed(
                                engine.groupMessageIntent(
                                    host = relayY.url,
                                    groupId = groupId,
                                    content = "compose on Y",
                                ),
                            )
                        val acked =
                            withTimeoutOrNull(10_000) {
                                receipt.status.first { it is WriteStatus.Acked }
                            }
                        assertTrue(acked is WriteStatus.Acked)
                        val wire = relayY.awaitEvent()
                        assertTrue(
                            wire.contains("[\"previous\",\"${yId.take(8)}\"]"),
                            "Y-delivered evidence must contribute previous: $wire",
                        )
                        assertFalse(
                            wire.contains(xId.take(8)),
                            "X-only evidence must not influence a Y-pinned write: $wire",
                        )
                    }
                }
            }
        }
    }

    @Test
    fun groupMessageIntentRejectsMalformedTypedRecipients() {
        NMPEngine(NMPConfig()).use { engine ->
            engine.setActiveAccount("a".repeat(64))
            val error =
                assertThrows(NMPError.InvalidPublicKey::class.java) {
                    engine.groupMessageIntent(
                        host = "wss://group-host.example.com",
                        groupId = "group-a",
                        content = "hello",
                        recipients = listOf("not-a-pubkey"),
                    )
                }
            assertEquals("not-a-pubkey", error.got)
        }
    }

    @Test
    fun publishComposedTakesTheIntentExactlyOnce() {
        runBlocking {
            NMPEngine(NMPConfig()).use { engine ->
                val pubkey = "a".repeat(64)
                engine.setActiveAccount(pubkey)

                val intent =
                    engine.groupMessageIntent(
                        host = "wss://group-host.example.com",
                        groupId = "group-a",
                        content = "hi",
                    )

                val receipt = engine.publishComposed(intent)
                val first = withTimeoutOrNull(5_000) { receipt.status.first() }
                assertEquals(WriteStatus.Accepted, first)

                try {
                    engine.publishComposed(intent)
                    fail<Unit>("expected the second publishComposed call to throw")
                } catch (e: NMPError.IntentAlreadyConsumed) {
                    // expected
                }
            }
        }
    }
}
