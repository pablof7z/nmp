// #1189: `NMPLiveQuery`'s identity is the CANONICAL branch set, exactly as it
// is in Rust -- not the order an app happened to type the branches in, and not
// a list a duplicate can hide in until the boundary silently drops it.
//
// #1108 required this and nothing named `LiveQuery` was ever tested natively,
// which is how an ordered-list data class with a public constructor shipped on
// both SDKs. These tests are that missing proof. No network: every assertion
// here is about declaration, so the one observation opened below declares
// cache-only branches and reads its first delivered evidence.
package com.nmp.sdk

import kotlinx.coroutines.flow.first
import kotlinx.coroutines.runBlocking
import kotlinx.coroutines.withTimeout
import org.junit.jupiter.api.Assertions.assertEquals
import org.junit.jupiter.api.Assertions.assertNotEquals
import org.junit.jupiter.api.Assertions.assertNull
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.assertThrows

class NMPLiveQueryTest {
    private fun branch(
        relay: String,
        freshness: NMPFreshness = NMPFreshness.Live,
    ): NMPLiveQuery =
        NMPLiveQuery.single(
            NMPDemand(
                selection = NMPFilter(kinds = listOf(1u)),
                source = NMPSourceAuthority.Pinned(setOf(relay)),
                freshness = freshness,
            ),
        )

    /** The same two branches typed in either order are ONE query. An app that
     * memoizes or `distinctUntilChanged`s on the declaration must not reopen
     * an observation NMP considers unchanged. */
    @Test
    fun declarationOrderDoesNotChangeIdentity() {
        val a = branch("wss://a.example.com")
        val b = branch("wss://b.example.com")

        val oneWay = NMPLiveQuery.union(listOf(a, b))
        val otherWay = NMPLiveQuery.union(listOf(b, a))

        assertEquals(oneWay, otherWay)
        assertEquals(oneWay.hashCode(), otherWay.hashCode())
        assertEquals(oneWay.branches, otherWay.branches)
        assertEquals(2, oneWay.branches.size)
    }

    /** A branch declared twice owns one branch -- as it does in Rust, where it
     * also owns one evidence entry and one refcount claim. */
    @Test
    fun duplicateBranchAppearsOnce() {
        val a = branch("wss://a.example.com")

        val query = NMPLiveQuery.union(listOf(a, a))

        assertEquals(1, query.branches.size)
        assertEquals(a, query)
    }

    /** Nested input flattens rather than nesting, so an ergonomically grouped
     * declaration is the same value as a flat one. */
    @Test
    fun nestedInputFlattensIntoOneCanonicalSet() {
        val a = branch("wss://a.example.com")
        val b = branch("wss://b.example.com")
        val c = branch("wss://c.example.com")

        val flat = NMPLiveQuery.union(listOf(a, b, c))
        val nested = NMPLiveQuery.union(listOf(c, NMPLiveQuery.union(listOf(b, a, b))))

        assertEquals(flat, nested)
        assertEquals(3, flat.branches.size)
    }

    /** The aggregate bound is part of the value, and a single branch never
     * carries one. */
    @Test
    fun theAggregateBoundIsPartOfTheValue() {
        val a = branch("wss://a.example.com")

        assertNull(a.aggregateResultLimit)
        assertEquals(7u, NMPLiveQuery.union(listOf(a), 7u).aggregateResultLimit)
        assertNotEquals(NMPLiveQuery.union(listOf(a), 7u), a)
    }

    /** The count the app reads off its own declaration is the count of
     * evidence entries the observation delivers. A duplicate that survived
     * locally would make these disagree. */
    @Test
    fun branchCountMatchesDeliveredEvidenceCount() {
        val a = branch("wss://a.example.com", NMPFreshness.CacheOnly)
        val b = branch("wss://b.example.com", NMPFreshness.CacheOnly)
        val query = NMPLiveQuery.union(listOf(a, b, a))

        assertEquals(2, query.branches.size)
        NMPEngine(NMPConfig()).use { engine ->
            runBlocking {
                val batch = withTimeout(30_000) { engine.observe(query).first() }
                assertEquals(query.branches.size, batch.evidence.size)
            }
        }
    }

    /** Each unobservable declaration is refused as its own typed error, at
     * construction -- before a handle, a graph claim or a wire request
     * exists. */
    @Test
    fun everyRefusalIsItsOwnTypedError() {
        val a = branch("wss://a.example.com")

        assertThrows<NMPError.EmptyQueryUnion> { NMPLiveQuery.union(emptyList()) }
        assertThrows<NMPError.AggregateResultLimitZero> { NMPLiveQuery.union(listOf(a), 0u) }

        val bounded = NMPLiveQuery.union(listOf(a), 3u)
        assertThrows<NMPError.NestedAggregateResultLimit> { NMPLiveQuery.union(listOf(bounded)) }

        val ceiling = NMPLiveQuery.MAX_BRANCHES
        val overCap = (0u..ceiling).map { branch("wss://relay-$it.example.com") }
        val refusal =
            assertThrows<NMPError.TooManyQueryBranches> { NMPLiveQuery.union(overCap) }
        assertEquals(ceiling.toULong() + 1uL, refusal.requested)
        assertEquals(ceiling.toULong(), refusal.maximum)
    }
}
