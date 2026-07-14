// The bounded LIVE test (#40) -- the Kotlin/JVM counterpart of
// LiveRelayTests.swift: proves the whole Kotlin -> uniffi.nmp_ffi ->
// nmp-engine -> real-relay path end to end, using ONLY the public
// `com.nmp.sdk` surface (no raw websocket code in this file). Every network
// wait is bounded (~30s) so this can never hang CI -- `withTimeoutOrNull`
// plus `Flow.first { predicate }` gives the same "race the first matching
// value against a hard timeout" shape Swift's `LiveRelayTests` builds by
// hand with `withTaskGroup`; Kotlin's own operator vocabulary gets there in
// two stock calls instead of a manual race -- a small but real Flow-fits-
// this-shape-well finding for #40's write-up.
package com.nmp.sdk

import kotlinx.coroutines.CompletableDeferred
import kotlinx.coroutines.Job
import kotlinx.coroutines.cancelAndJoin
import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.collect
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.launch
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeoutOrNull
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertFalse
import org.junit.jupiter.api.Assertions.assertTrue
import org.junit.jupiter.api.Assumptions.assumeTrue
import org.junit.jupiter.api.Test

class LiveRelayTest {
    companion object {
        /** fiatjaf -- a known, always-active npub, used only as a read
         * target. No secret key is used anywhere in this test:
         * `setActiveAccount` may re-root reads onto an account this
         * process holds no key for (read-only browsing is legal). */
        const val FIATJAF_HEX = "3bf0c63fcb93463407af97a5e5ee64fa883d107ef9e558472c4eb9aaaefa459d"
        val INDEXER_RELAYS = listOf("wss://purplepag.es", "wss://relay.primal.net")

        suspend fun firstNonEmptyBatch(flow: Flow<RowBatch>, timeoutMs: Long): List<Row>? =
            withTimeoutOrNull(timeoutMs) {
                flow.first { it.rows.isNotEmpty() }.rows
            }

        fun followFeed(): NMPFilter =
            NMPFilter(
                kinds = listOf(1u),
                authors =
                    NMPBinding.Derived(
                        inner = NMPFilter(kinds = listOf(3u), authors = NMPBinding.Reactive(NMPIdentityField.ActivePubkey)),
                        project = NMPSelector.Tag("p"),
                    ),
                limit = 50u,
            )
    }

    /** #442: the complete JVM/UniFFI engine lifecycle repeatedly returns
     * every owned runtime/transport worker before the next construction. */
    @Test
    fun repeatedEngineConstructionAndShutdown() {
        repeat(32) {
            NMPEngine(NMPConfig()).close()
        }
    }

    /** THE headline live proof: construct the engine from ONLY the two
     * operator indexer relays (no write-relay map -- there is no such field
     * anymore), add a read-only account for fiatjaf, and observe the
     * reactive follow-feed. This app never resolves a single relay itself
     * -- the engine discovers fiatjaf's own write relays live and re-routes
     * the content atom to them on its own. */
    @Test
    fun followFeedResolvesFromIndexerRelaysAlone() =
        runBlocking {
            NMPEngine(NMPConfig(indexerRelays = INDEXER_RELAYS)).use { engine ->
                engine.setActiveAccount(FIATJAF_HEX)

                val rows = firstNonEmptyBatch(engine.observe(followFeed()), timeoutMs = 30_000)
                assumeTrue(
                    rows != null,
                    "Observed no follow-feed rows within 30s from $INDEXER_RELAYS alone -- the " +
                        "indexers, or fiatjaf's follows' write relays, may be unreachable from this " +
                        "test environment. Package build + construction tests still pass " +
                        "independently of this network condition.",
                )

                assertTrue(rows!!.isNotEmpty(), "expected at least one real note")
                for (row in rows.take(5)) {
                    assertEquals(1u.toUShort(), row.kind)
                    assertFalse(row.id.isEmpty())
                }
            }
        }

    /** The diagnostic surface, proven live: once the follow feed has
     * actually produced rows, `observeDiagnostics()` must show a relay
     * whose `eventsByKind` reports a REAL received kind:1 count > 0 --
     * never fabricated, and matching what the row stream already proved
     * arrived. */
    @Test
    fun diagnosticsSnapshotShowsRealEventsByKindForTheFollowFeed() =
        runBlocking {
            NMPEngine(NMPConfig(indexerRelays = INDEXER_RELAYS)).use { engine ->
                engine.setActiveAccount(FIATJAF_HEX)

                val queryFlow = engine.observe(followFeed())
                val rowsReady = CompletableDeferred<List<Row>>()
                val queryJob =
                    launch {
                        queryFlow.collect { batch ->
                            if (batch.rows.isNotEmpty()) rowsReady.complete(batch.rows)
                        }
                    }
                var diagnosticsJob: Job? = null
                try {
                    val rows = withTimeoutOrNull(30_000) { rowsReady.await() }
                    assumeTrue(
                        rows != null,
                        "Observed no follow-feed rows within 30s from $INDEXER_RELAYS alone -- " +
                            "diagnostics has nothing real to report in this test environment.",
                    )

                    // Keep collecting `queryFlow` while diagnostics is sampled. `Flow.first`
                    // would cancel the callbackFlow (and its native query handle) as soon as
                    // rows arrived, removing the relay from the current diagnostics plan.
                    val snapshotReady = CompletableDeferred<DiagnosticsSnapshot>()
                    diagnosticsJob =
                        launch {
                            engine.observeDiagnostics().collect { snapshot ->
                                val hasReceivedKind1 =
                                    snapshot.relays.any { relay ->
                                        relay.eventsByKind.any { it.kind == 1u.toUShort() && it.count > 0u }
                                    }
                                if (hasReceivedKind1) snapshotReady.complete(snapshot)
                            }
                        }
                    val snapshot = withTimeoutOrNull(10_000) { snapshotReady.await() }
                    assertTrue(
                        snapshot != null,
                        "expected a diagnostics snapshot reporting a real kind:1 event count, once the " +
                            "follow feed had already produced rows",
                    )

                    assertTrue(snapshot!!.relays.isNotEmpty())
                    val hasReceivedKind1 =
                        snapshot.relays.any { relay ->
                            relay.eventsByKind.any { it.kind == 1u.toUShort() && it.count > 0u }
                        }
                    assertTrue(
                        hasReceivedKind1,
                        "at least one relay must show a real received kind:1 count, matching the rows " +
                            "already observed",
                    )
                } finally {
                    diagnosticsJob?.cancelAndJoin()
                    queryJob.cancelAndJoin()
                }
            }
        }

    /** The same self-bootstrapping proof for a LITERAL author set (no
     * derived binding involved at all): fiatjaf's own kind:1 notes, from a
     * fresh engine configured with ONLY the indexer relays. */
    @Test
    fun authorsOwnNotesArriveWithNoWriteRelayConfigured() =
        runBlocking {
            NMPEngine(NMPConfig(indexerRelays = INDEXER_RELAYS)).use { engine ->
                val notesFilter =
                    NMPFilter(kinds = listOf(1u), authors = NMPBinding.Literal(setOf(FIATJAF_HEX)), limit = 20u)

                val rows = firstNonEmptyBatch(engine.observe(notesFilter), timeoutMs = 30_000)
                assumeTrue(
                    rows != null,
                    "Observed no kind:1 notes for fiatjaf within 30s from $INDEXER_RELAYS alone -- " +
                        "his resolved write relays may be unreachable from this test environment.",
                )

                assertTrue(rows!!.isNotEmpty(), "expected at least one real note")
                for (row in rows.take(5)) {
                    assertEquals(1u.toUShort(), row.kind)
                    assertEquals(FIATJAF_HEX, row.pubkey)
                    assertFalse(row.id.isEmpty())
                    assertFalse(row.content.isEmpty())
                }
            }
        }
}
