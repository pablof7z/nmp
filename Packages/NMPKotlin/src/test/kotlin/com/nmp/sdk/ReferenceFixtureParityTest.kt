package com.nmp.sdk

import java.io.File
import kotlinx.serialization.json.Json
import kotlinx.serialization.json.JsonArray
import kotlinx.serialization.json.JsonElement
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
    fun sharedNip19FixturesMatchKotlinTargetsAndDemandPlans() {
        val corpus =
            Json.parseToJsonElement(File(fixturePath()).readText()).jsonObject
        assertEquals(1, corpus.getValue("schema").jsonPrimitive.content.toInt())

        corpus.getValue("cases").jsonArray.forEach { element ->
            val fixture = element.jsonObject
            val name = fixture.getValue("name").jsonPrimitive.content
            val input = fixture.getValue("input").jsonPrimitive.content
            when (fixture.getValue("outcome").jsonPrimitive.content) {
                "public" -> {
                    val expectedTarget = fixture.getValue("target")
                    assertEquals(expectedTarget, normalize(decodeNostrEntity(input)), "$name entity")
                    if (input.startsWith("nostr:")) {
                        assertEquals(
                            expectedTarget,
                            normalize(decodeNostrEntity(input.removePrefix("nostr:"))),
                            "$name nostr URI and bare forms",
                        )
                    }
                    val target = parseNostrContent(input).references.single().target
                    assertEquals(expectedTarget, normalize(target), "$name target")
                    assertEquals(
                        fixture.getValue("plan"),
                        normalize(referenceDemandPlan(target)),
                        "$name demand plan",
                    )
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
    buildJsonObject {
        when (target) {
            is NostrReferenceTarget.Profile -> {
                put("kind", JsonPrimitive("profile"))
                put("key", JsonPrimitive(target.key))
                putNullableString("pubkey", target.pubkey)
                putNullableString("id", null)
                putNullableString("author_hint", null)
                putNullableInt("kind_hint", null)
                putNullableInt("address_kind", null)
                putNullableString("author", null)
                putNullableString("identifier", null)
                put("relay_hints", strings(target.relayHints))
            }
            is NostrReferenceTarget.Event -> {
                put("kind", JsonPrimitive("event"))
                put("key", JsonPrimitive(target.key))
                putNullableString("pubkey", null)
                putNullableString("id", target.id)
                putNullableString("author_hint", target.authorHint)
                putNullableInt("kind_hint", target.kindHint?.toInt())
                putNullableInt("address_kind", null)
                putNullableString("author", null)
                putNullableString("identifier", null)
                put("relay_hints", strings(target.relayHints))
            }
            is NostrReferenceTarget.Address -> {
                put("kind", JsonPrimitive("address"))
                put("key", JsonPrimitive(target.key))
                putNullableString("pubkey", null)
                putNullableString("id", null)
                putNullableString("author_hint", null)
                putNullableInt("kind_hint", null)
                putNullableInt("address_kind", target.kind.toInt())
                putNullableString("author", target.author)
                putNullableString("identifier", target.identifier)
                put("relay_hints", strings(target.relayHints))
            }
        }
    }

private fun normalize(entity: NostrEntity): JsonObject =
    normalize(
        when (entity) {
            is NostrEntity.Pubkey -> NostrReferenceTarget.Profile(entity.pubkey)
            is NostrEntity.Profile -> NostrReferenceTarget.Profile(entity.pubkey, entity.relays)
            is NostrEntity.EventId -> NostrReferenceTarget.Event(entity.id)
            is NostrEntity.Event ->
                NostrReferenceTarget.Event(entity.id, entity.author, entity.kind, entity.relays)
            is NostrEntity.Coordinate ->
                NostrReferenceTarget.Address(
                    entity.kind,
                    entity.author,
                    entity.identifier,
                    entity.relays,
                )
        },
    )

private fun normalize(plan: NostrReferenceDemandPlan): JsonObject =
    buildJsonObject {
        put("target_key", JsonPrimitive(plan.targetKey))
        put("canonical", normalize(plan.canonical))
        put("helpers", buildJsonArray { plan.helpers.forEach { add(normalize(it)) } })
        put("discarded_relay_hints", JsonPrimitive(plan.discardedRelayHints.toLong()))
    }

private fun normalize(demand: NMPDemand): JsonObject =
    buildJsonObject {
        put("selection", normalize(demand.selection))
        put(
            "source",
            buildJsonObject {
                when (val source = demand.source) {
                    NMPSourceAuthority.AuthorOutboxes -> {
                        put("kind", JsonPrimitive("author_outboxes"))
                        put("relays", JsonArray(emptyList()))
                    }
                    NMPSourceAuthority.Public -> {
                        put("kind", JsonPrimitive("public"))
                        put("relays", JsonArray(emptyList()))
                    }
                    is NMPSourceAuthority.Pinned -> {
                        put("kind", JsonPrimitive("pinned"))
                        put("relays", strings(source.relays.sorted()))
                    }
                }
            },
        )
        put(
            "access",
            JsonPrimitive(
                when (val access = demand.access) {
                    NMPAccessContext.Public -> "public"
                    is NMPAccessContext.Nip42 -> "nip42:${access.publicKey}"
                },
            ),
        )
        put(
            "cache",
            JsonPrimitive(
                when (demand.cache) {
                    NMPCacheMode.Agnostic -> "agnostic"
                    NMPCacheMode.Strict -> "strict"
                },
            ),
        )
        put(
            "freshness",
            JsonPrimitive(
                when (val freshness = demand.freshness) {
                    NMPFreshness.Live -> "live"
                    is NMPFreshness.MaxAge -> "max_age:${freshness.seconds}"
                    NMPFreshness.CacheOnly -> "cache_only"
                },
            ),
        )
    }

private fun normalize(filter: NMPFilter): JsonObject =
    buildJsonObject {
        put(
            "kinds",
            buildJsonArray {
                filter.kinds.orEmpty().sorted().forEach { add(JsonPrimitive(it.toInt())) }
            },
        )
        put("authors", strings(filter.authors.literalValues()))
        put("ids", strings(filter.ids.literalValues()))
        put(
            "tags",
            buildJsonObject {
                filter.tags.entries.sortedBy { it.key }.forEach { (name, binding) ->
                    put(name.toString(), strings(binding.literalValues()))
                }
            },
        )
        putNullableLong("since", filter.since?.toLong())
        putNullableLong("until", filter.until?.toLong())
        putNullableInt("limit", filter.limit?.toInt())
    }

private fun NMPBinding?.literalValues(): List<String> =
    when (this) {
        null -> emptyList()
        is NMPBinding.Literal -> values.sorted()
        else -> error("reference plan emitted a non-literal binding: $this")
    }

private fun strings(values: Iterable<String>): JsonArray =
    buildJsonArray { values.forEach { add(JsonPrimitive(it)) } }

private fun JsonObjectBuilder.putNullableString(name: String, value: String?) {
    put(name, value?.let(::JsonPrimitive) ?: JsonNull)
}

private fun JsonObjectBuilder.putNullableInt(name: String, value: Int?) {
    put(name, value?.let(::JsonPrimitive) ?: JsonNull)
}

private fun JsonObjectBuilder.putNullableLong(name: String, value: Long?) {
    put(name, value?.let(::JsonPrimitive) ?: JsonNull)
}
