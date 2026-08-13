package com.nmp.qualification

import com.nmp.sdk.NMPConfig
import com.nmp.sdk.NMPEngine
import com.nmp.sdk.NMPPrivateKey
import com.nmp.sdk.OutboxRoutingConfig
import com.nmp.sdk.RelayState
import com.nmp.sdk.WriteIntent
import com.nmp.sdk.WriteOutcome
import com.nmp.sdk.WritePayload
import com.nmp.sdk.WriteRouting
import kotlinx.coroutines.runBlocking
import java.nio.file.Files
import java.nio.file.Path

fun main(args: Array<String>) = runBlocking {
    val manifest = Files.readString(Path.of(args[0]))
    val value: (String) -> String = { key ->
        Regex("\\\"$key\\\"\\s*:\\s*\\\"([^\\\"]+)\\\"")
            .find(manifest)?.groupValues?.get(1) ?: error("missing $key")
    }
    NMPEngine(
        NMPConfig(
            outboxRouting = OutboxRoutingConfig(listOf(value("indexer"))),
            allowedLocalRelayHosts = listOf("localhost", "127.0.0.1"),
        ),
    ).use { engine ->
        val privateKey = NMPPrivateKey(value("secret_key").hexBytes())
        engine.session.add(privateKey, makeCurrent = true)
        val result = engine.publish(
            WriteIntent(
                payload = WritePayload.Event(1u, content = "kotlin prepared cold discovery"),
                routing = WriteRouting.Auto,
            ),
        ).result()
        check(result.outcome == WriteOutcome.Settled)
        check(result.relays.size == 1)
        check(result.relays.single().relay == value("outbox"))
        check(result.relays.single().state == RelayState.Published)
    }
    println("PASS kotlin prepared outbox routing cold discovery")
}

private fun String.hexBytes(): ByteArray {
    require(length % 2 == 0) { "invalid hex private key" }
    return ByteArray(length / 2) { index ->
        substring(index * 2, index * 2 + 2).toIntOrNull(16)?.toByte()
            ?: error("invalid hex private key")
    }
}
