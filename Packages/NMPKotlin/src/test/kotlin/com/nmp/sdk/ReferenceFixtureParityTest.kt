package com.nmp.sdk

import java.io.File
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonNull
import kotlinx.serialization.json.JsonObject
import kotlinx.serialization.json.JsonObjectBuilder
import kotlinx.serialization.json.JsonPrimitive
import kotlinx.serialization.json.buildJsonArray
import kotlinx.serialization.json.buildJsonObject
import kotlinx.serialization.json.jsonArray
import kotlinx.serialization.json.jsonObject
import kotlinx.serialization.json.jsonPrimitive
import org.junit.jupiter.api.Test
import kotlin.test.assertEquals
import kotlin.test.assertFailsWith
import kotlin.test.assertTrue

class ReferenceFixtureParityTest {
    @Test
    fun sharedNip19FixturesPreserveExactKotlinLocators() {
        val corpus = Json.parseToJsonElement(File(fixturePath()).readText()).jsonObject
        assertEquals(2, corpus.getValue("schema").jsonPrimitive.content.toInt())

        corpus.getValue("cases").jsonArray.forEach { element ->
            val fixture = element.jsonObject
            val name = fixture.getValue("name").jsonPrimitive.content
            val input = fixture.getValue("input").jsonPrimitive.content
            when (fixture.getValue("outcome").jsonPrimitive.content) {
                "public" -> {
                    val expected = fixture.getValue("locator")
                    assertEquals(expected, normalize(decodeNostrEntity(input)), "$name entity")
                    if (input.startsWith("nostr:")) {
                        assertEquals(
                            expected,
                            normalize(decodeNostrEntity(input.removePrefix("nostr:"))),
                            "$name nostr URI and bare forms",
                        )
                    }
                    val locator = parseNostrContent(input).references.single().target
                    assertEquals(expected, normalize(locator), "$name content locator")
                }
                "secret_key" -> assertNonActionableSecret(name, input)
                "malformed" -> assertNonActionableMalformed(name, input)
                else -> error("unknown shared fixture outcome for $name")
            }
        }
    }

    private fun assertNonActionableSecret(name: String, input: String) {
        assertNonActionableContent(name, input)
        assertFailsWith<NMPError.NostrEntitySecretKeyRejected> {
            decodeNostrEntity(input)
        }
    }

    private fun assertNonActionableMalformed(name: String, input: String) {
        assertNonActionableContent(name, input)
        assertFailsWith<NMPError.InvalidNostrEntity> {
            decodeNostrEntity(input)
        }
    }

    private fun assertNonActionableContent(name: String, input: String) {
        val document = parseNostrContent(input)
        assertTrue(document.references.isEmpty(), name)
        val visible =
            document.blocks
                .flatMap { it.inlines }
                .filterIsInstance<NostrContentInline.Text>()
                .joinToString(separator = "") { it.text }
        assertEquals(input, visible, name)
    }
}

private fun fixturePath(): String =
    checkNotNull(System.getProperty("nmp.referenceFixturePath")) {
        "Gradle must provide the shared reference fixture path"
    }

private fun normalize(target: NostrReferenceTarget): JsonObject =
    when (target) {
        is NostrReferenceTarget.Pubkey ->
            normalized("pubkey", target.pubkey, null, null, null, null, emptyList())
        is NostrReferenceTarget.Profile ->
            normalized("profile", target.pubkey, null, null, null, null, target.relayHints)
        is NostrReferenceTarget.EventId ->
            normalized("event_id", null, target.id, null, null, null, emptyList())
        is NostrReferenceTarget.Event ->
            normalized(
                "event",
                null,
                target.id,
                target.authorHint,
                target.kindHint?.toInt(),
                null,
                target.relayHints,
            )
        is NostrReferenceTarget.Coordinate ->
            normalized(
                "coordinate",
                null,
                null,
                target.author,
                target.kind.toInt(),
                target.identifier,
                target.relayHints,
            )
    }

private fun normalize(entity: NostrEntity): JsonObject =
    when (entity) {
        is NostrEntity.Pubkey ->
            normalized("pubkey", entity.pubkey, null, null, null, null, emptyList())
        is NostrEntity.Profile ->
            normalized("profile", entity.pubkey, null, null, null, null, entity.relays)
        is NostrEntity.EventId ->
            normalized("event_id", null, entity.id, null, null, null, emptyList())
        is NostrEntity.Event ->
            normalized(
                "event",
                null,
                entity.id,
                entity.author,
                entity.kind?.toInt(),
                null,
                entity.relays,
            )
        is NostrEntity.Coordinate ->
            normalized(
                "coordinate",
                null,
                null,
                entity.author,
                entity.kind.toInt(),
                entity.identifier,
                entity.relays,
            )
    }

private fun normalized(
    variant: String,
    pubkey: String?,
    id: String?,
    author: String?,
    eventKind: Int?,
    identifier: String?,
    relays: List<String>,
): JsonObject =
    buildJsonObject {
        put("variant", JsonPrimitive(variant))
        putNullableString("pubkey", pubkey)
        putNullableString("id", id)
        putNullableString("author", author)
        putNullableInt("event_kind", eventKind)
        putNullableString("identifier", identifier)
        put("relays", strings(relays))
    }

private fun strings(values: Iterable<String>): JsonArray =
    buildJsonArray { values.forEach { add(JsonPrimitive(it)) } }

private fun JsonObjectBuilder.putNullableString(name: String, value: String?) {
    put(name, value?.let(::JsonPrimitive) ?: JsonNull)
}

private fun JsonObjectBuilder.putNullableInt(name: String, value: Int?) {
    put(name, value?.let(::JsonPrimitive) ?: JsonNull)
}
