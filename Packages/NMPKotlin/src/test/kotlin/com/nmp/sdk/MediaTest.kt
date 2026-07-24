package com.nmp.sdk

import java.io.ByteArrayOutputStream
import java.io.File
import java.net.InetAddress
import java.net.ServerSocket
import java.nio.charset.StandardCharsets
import java.time.Instant
import java.util.concurrent.atomic.AtomicReference
import kotlin.concurrent.thread
import kotlinx.coroutines.runBlocking
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Test
import uniffi.nmp_ffi.FfiPictureComposeException

private class LocalBlossomServer(
    descriptor: JsonObject,
    sha256Override: String? = null,
) : AutoCloseable {
    private val server = ServerSocket(0, 1, InetAddress.getByName("127.0.0.1"))
    private val failure = AtomicReference<Throwable?>()
    private val responseBody =
        (
            """{"url":"${descriptor.text("url")}","sha256":"${sha256Override ?: descriptor.text("sha256")}",""" +
                """"size":${descriptor.number("size")},"type":"${descriptor.text("mime_type")}"}"""
        ).toByteArray(StandardCharsets.UTF_8)
    private val worker =
        thread(name = "nmp-kotlin-media-fixture", isDaemon = true) {
            try {
                server.accept().use { socket ->
                    val input = socket.getInputStream()
                    val headers = ByteArrayOutputStream()
                    var matched = 0
                    val terminator = byteArrayOf(13, 10, 13, 10)
                    while (matched < terminator.size) {
                        val byte = input.read()
                        check(byte >= 0) { "Blossom request ended before headers" }
                        headers.write(byte)
                        matched = if (byte.toByte() == terminator[matched]) matched + 1 else 0
                    }
                    val contentLength =
                        headers
                            .toString(StandardCharsets.US_ASCII)
                            .lineSequence()
                            .mapNotNull { line ->
                                val pieces = line.split(':', limit = 2)
                                if (
                                    pieces.size == 2 &&
                                    pieces[0].trim().equals("content-length", ignoreCase = true)
                                ) {
                                    pieces[1].trim().toIntOrNull()
                                } else {
                                    null
                                }
                            }.firstOrNull() ?: 0
                    repeat(contentLength) {
                        check(input.read() >= 0) { "Blossom request ended before body" }
                    }

                    val responseHeaders =
                        ("HTTP/1.1 200 OK\r\n" +
                            "Content-Type: application/json\r\n" +
                            "Content-Length: ${responseBody.size}\r\n" +
                            "Connection: close\r\n\r\n")
                            .toByteArray(StandardCharsets.US_ASCII)
                    socket.getOutputStream().apply {
                        write(responseHeaders)
                        write(responseBody)
                        flush()
                    }
                }
            } catch (error: Throwable) {
                if (!server.isClosed) failure.set(error)
            }
        }

    val serverUrl: String = "http://127.0.0.1:${server.localPort}"

    override fun close() {
        server.close()
        worker.join(2_000)
        failure.get()?.let { throw AssertionError("local Blossom fixture failed", it) }
    }
}

class MediaTest {
    /** Shared fixture falsifier: Kotlin retains the opaque upload witness and
     * emits the same governed kind/tags/content and typed write inputs as
     * Rust/FFI/Swift. */
    @Test
    fun sharedVerifiedUploadPictureFixture() =
        runBlocking {
            val fixture = mediaFixture()
            val descriptor = fixture.objectAt("descriptor")
            val image = fixture.objectAt("image")
            val post = fixture.objectAt("post")
            val expected = fixture.objectAt("expected")
            val secret = fixture.text("secret_key_hex")
            val author = fixture.text("author_pubkey_hex")
            val blob = fixture.text("blob_utf8").toByteArray(StandardCharsets.UTF_8)
            val now = Instant.now().epochSecond.toULong()

            NMPEngine(NMPConfig()).use { engine ->
                assertEquals(author, engine.addAccount(secret).publicKey)
                engine.setActiveAccount(author)

                val authorizationDraft =
                    blossomUploadAuthorizationDraft(
                        authorPubkeyHex = author,
                        blobSha256Hex = descriptor.text("sha256"),
                        createdAt = now - 5u,
                        expiration = now + 300u,
                        description = "NIP-68 parity upload",
                    )
                val signedAuthorization = engine.signEvent(authorizationDraft.signRequest)
                val authorization =
                    BlossomAuthorization.validate(
                        signedEvent = signedAuthorization,
                        verb = BlossomVerb.UPLOAD,
                        blobSha256Hex = descriptor.text("sha256"),
                        now = now,
                    )

                LocalBlossomServer(descriptor).use { server ->
                    val client =
                        BlossomClient(
                            BlossomClientConfig(
                                allowedLocalHosts = listOf("127.0.0.1"),
                                requestDeadlineSeconds = 5u,
                            ),
                        )
                    val verified =
                        client.upload(
                            serverUrl = server.serverUrl,
                            blob = blob,
                            contentType = descriptor.text("mime_type"),
                            authorization = authorization,
                        )
                    assertEquals(descriptor.text("sha256"), verified.descriptor.sha256)
                    assertEquals(descriptor.text("url"), verified.descriptor.url)

                    val tamperedHash = "0".repeat(64)
                    LocalBlossomServer(descriptor, tamperedHash).use { tamperedServer ->
                        val failure =
                            try {
                                client.upload(
                                    serverUrl = tamperedServer.serverUrl,
                                    blob = blob,
                                    contentType = descriptor.text("mime_type"),
                                    authorization = authorization,
                                )
                                null
                            } catch (error: BlossomUploadError) {
                                error
                            }
                        assertEquals(
                            BlossomUploadError.Sha256Mismatch(
                                descriptor.text("sha256"),
                                tamperedHash,
                            ),
                            failure,
                        )
                    }

                    val draft =
                        composePicture(
                            authorPubkeyHex = author,
                            createdAt = fixture.number("created_at").toULong(),
                            images =
                                listOf(
                                    PictureImage(
                                        upload = verified,
                                        dimensions =
                                            ImageDimensions(
                                                image.number("width").toUInt(),
                                                image.number("height").toUInt(),
                                            ),
                                        alt = image.text("alt"),
                                        blurhash = image.text("blurhash"),
                                        thumbhash = image.text("thumbhash"),
                                        fallbacks = image.strings("fallbacks"),
                                    ),
                                ),
                            post =
                                PicturePost(
                                    title = post.text("title"),
                                    description = post.text("description"),
                                    contentWarning =
                                        PictureContentWarning(post.text("content_warning_reason")),
                                    hashtags = post.strings("hashtags"),
                                ),
                        )

                    val expectedTags = expected.tags("tags")
                    assertEquals(expected.number("kind").toUShort(), draft.kind)
                    assertEquals(expectedTags, draft.tags)
                    assertEquals(expected.text("content"), draft.content)
                    assertEquals(expected.text("unsigned_event_json"), draft.unsignedEventJson)
                    assertEquals(expectedTags, draft.signRequest.tags)

                    val intent =
                        draft.writeIntent(
                            durability = Durability.Durable,
                            routing = WriteRouting.AuthorOutbox,
                            correlation = "nip68-parity",
                        )
                    assertEquals("durable", expected.text("durability"))
                    assertEquals("author_outbox", expected.text("routing"))
                    assertEquals(Durability.Durable, intent.durability)
                    assertTrue(intent.routing is WriteRouting.AuthorOutbox)
                    assertEquals("nip68-parity", intent.correlation)
                    val payload = intent.payload as WritePayload.Unsigned
                    assertEquals(author, payload.pubkey)
                    assertEquals(fixture.number("created_at").toULong(), payload.createdAt)
                    assertEquals(expected.number("kind").toUShort(), payload.kind)
                    assertEquals(expectedTags, payload.tags)
                    assertEquals(expected.text("content"), payload.content)

                    val signedPicture = engine.signEvent(draft.signRequest)
                    assertEquals(expected.text("event_id"), signedPicture.id)
                    assertEquals(author, signedPicture.pubkey)
                    assertEquals(expected.number("kind").toUShort(), signedPicture.kind)
                    assertEquals(expectedTags, signedPicture.tags)
                    assertEquals(expected.text("content"), signedPicture.content)
                }
            }
        }

    @Test
    fun everyComposeFailureMapsWithoutCollapsingDomains() {
        assertEquals(
            PictureComposeError.InvalidAuthorPubkey("bad"),
            PictureComposeError.from(FfiPictureComposeException.InvalidAuthorPubkey("bad")),
        )
        assertEquals(
            PictureComposeError.NoImages,
            PictureComposeError.from(FfiPictureComposeException.NoImages()),
        )
        assertEquals(
            PictureComposeError.ImageMissingMimeType,
            PictureComposeError.from(FfiPictureComposeException.ImageMissingMimeType()),
        )
        assertEquals(
            PictureComposeError.EmptyHashtag,
            PictureComposeError.from(FfiPictureComposeException.EmptyHashtag()),
        )
    }
}

private fun mediaFixture(): JsonObject {
    val path =
        System.getProperty("nmp.mediaParityFixturePath")
            ?: error("Gradle must provide the shared media parity fixture path")
    return Json.parseToJsonElement(File(path).readText()).jsonObject
}

private fun JsonObject.text(key: String): String = getValue(key).jsonPrimitive.content

private fun JsonObject.number(key: String): Long = getValue(key).jsonPrimitive.content.toLong()

private fun JsonObject.objectAt(key: String): JsonObject = getValue(key).jsonObject

private fun JsonObject.strings(key: String): List<String> =
    getValue(key).jsonArray.map { it.jsonPrimitive.content }

private fun JsonObject.tags(key: String): List<List<String>> =
    (getValue(key) as JsonArray).map { row ->
        row.jsonArray.map { it.jsonPrimitive.content }
    }
