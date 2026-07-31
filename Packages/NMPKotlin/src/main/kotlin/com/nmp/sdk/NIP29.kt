// The NIP-29 door, in ergonomic Kotlin shape (#1033): a relay scope you name
// once, narrowed to a group when you want a specific one. Mirrors
// NIP29.swift.
//
//   val scope = NMPRelayScope.on(listOf("wss://relay-a.example.com"))
//   val mine = NMPGroupPredicate.memberListIncludes(NMPBinding.Reactive(NMPIdentityField.ActivePubkey))
//   val query = scope.groupsWhere(mine)                    // one branch per host
//
//   val group = scope.group("photographers")                // narrows, contacts nothing
//   val query = group.read(NMPFilter(kinds = listOf(9u)))    // #h-scoped, per-host branches
//   val status = group.publish(engine, authorPubkeyHex = pubkeyHex, kind = 9u, content = "hi")
//   status.collect { ... }
//
// `NMPRelayScope`/`NMPGroup`/`NMPGroupPredicate` wrap the opaque
// `FfiRelayScope`/`FfiGroup`/`FfiGroupPredicate` UniFFI objects exactly like
// `BlossomAuthorization` wraps `FfiBlossomAuthorization` in Blossom.kt -- a
// proven Rust value carried across the boundary, never a second mirrored
// copy of NIP-29's own vocabulary. Neither type exposes its retained hosts
// or group id back out.
//
// Deliberately absent, same as before #1033: a fixed group-content kind
// catalog and a kind:9 composer -- NIP-29 owns neither; C7 and client
// notification policy remain independently optional (#838). Also absent:
// any second projection of a NIP-51 Simple-groups entry -- `NIP51.kt` keeps
// that one shape.
//
// Deleted in this change, no alias: `groupDiscoveryDemand(host)` and
// `Group`'s single-host constructor. A group can live on more than one
// relay; the single-host door is gone, not deprecated.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiGroup
import uniffi.nmp_ffi.FfiGroupPredicate
import uniffi.nmp_ffi.FfiRelayScope
import uniffi.nmp_ffi.FfiSignedEvent
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.NmpGroupReceiptStream
import uniffi.nmp_ffi.adminListIncludes as ffiAdminListIncludes
import uniffi.nmp_ffi.memberListIncludes as ffiMemberListIncludes

/** The relays a NIP-29 group lives on -- named once, retained privately, and
 * never asked for again (`nmp::nip29::RelayScope`/`FfiRelayScope` mirror). */
class NMPRelayScope private constructor(internal val ffi: FfiRelayScope) {
    /** Narrow to one group id, keeping the same hosts. Contacts nothing. */
    fun group(groupId: String): NMPGroup = NMPGroup(ffi.group(groupId))

    /** Groups on these relays matching a composable discovery predicate. One
     * complete branch per host, folded into ONE [NMPLiveQuery] --
     * `NMPEngine.observe` takes it directly, never a per-host demand list
     * the app has to merge itself. */
    fun groupsWhere(predicate: NMPGroupPredicate): NMPLiveQuery =
        NMPLiveQuery.from(nmpRethrowing { ffi.groupsWhere(predicate.ffi) })

    companion object {
        /** Name the relays a NIP-29 group lives on. Each host is parsed with
         * the same rule every other relay-URL input in this package uses
         * (`NMPError.InvalidRelayUrl`); an empty set throws
         * `NMPError.EmptyRelayScope` -- a group must be hosted somewhere. */
        fun on(hosts: List<String>): NMPRelayScope =
            NMPRelayScope(nmpRethrowing { FfiRelayScope.on(hosts) })
    }
}

/** One NIP-29 group, on the relays its scope named (`nmp::nip29::Group`/
 * `FfiGroup` mirror). An identity, not a subscription: obtaining one (via
 * [NMPRelayScope.group]) contacts nothing. The same value serves every read
 * and every write for a room's whole lifetime. */
class NMPGroup internal constructor(internal val ffi: FfiGroup) {
    /** Mint the read declaration for an app-supplied selection. A selection
     * that already constrains `#h` throws
     * `NMPError.GroupCallerSuppliedContextConstraint` -- the retained group
     * id is the sole semantic source of that row. Hand the result to
     * `NMPEngine.observe`. */
    fun read(selection: NMPFilter): NMPLiveQuery =
        NMPLiveQuery.from(nmpRethrowing { ffi.read(selection.toFfi()) })

    /** Ask whether an already-signed event belongs to this group, without
     * building a write out of it. */
    fun validateContext(event: NMPSignedEvent) {
        nmpRethrowing { ffi.validateContext(event.toFfiSignedEvent()) }
    }

    /** Publish an unsigned draft into the group, as [authorPubkeyHex] (exact
     * decoded hex, never the active-account selector -- a semantic group
     * write freezes who is writing at composition time, #878). */
    fun publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        content: String = "",
        createdAt: ULong? = null,
    ): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing {
                ffi.publish(
                    engine.ffi,
                    authorPubkeyHex,
                    FfiEventBuilder(kind, tags, content, createdAt),
                )
            },
        )

    /** Publish an ALREADY-SIGNED event into the group. The `h` it already
     * carries is validated, never appended or repaired -- see
     * [validateContext]'s doc for the exact refusals. */
    fun publishSigned(engine: NMPEngine, event: NMPSignedEvent): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.publishSigned(engine.ffi, event.toFfiSignedEvent()) },
        )

    /** kind:9021 -- ask to join. Publishable with no subscription at all. */
    fun joinRequest(
        engine: NMPEngine,
        authorPubkeyHex: String,
        inviteCode: String? = null,
    ): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.joinRequest(engine.ffi, authorPubkeyHex, inviteCode) },
        )

    /** kind:9022 -- leave. */
    fun leaveRequest(engine: NMPEngine, authorPubkeyHex: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(nmpRethrowing { ffi.leaveRequest(engine.ffi, authorPubkeyHex) })

    /** kind:9000 -- add a member, optionally with a role. */
    fun addUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String,
        role: String? = null,
    ): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.addUser(engine.ffi, authorPubkeyHex, pubkeyHex, role) },
        )

    /** kind:9001 -- remove a member. */
    fun removeUser(engine: NMPEngine, authorPubkeyHex: String, pubkeyHex: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.removeUser(engine.ffi, authorPubkeyHex, pubkeyHex) },
        )

    /** kind:9002 -- set the group's display fields. An omitted field emits
     * no tag at all, so it is left untouched rather than cleared. */
    fun editMetadata(
        engine: NMPEngine,
        authorPubkeyHex: String,
        name: String? = null,
        about: String? = null,
    ): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.editMetadata(engine.ffi, authorPubkeyHex, name, about) },
        )

    /** kind:9005 -- delete one group-hosted event. */
    fun deleteEvent(engine: NMPEngine, authorPubkeyHex: String, eventId: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.deleteEvent(engine.ffi, authorPubkeyHex, eventId) },
        )

    /** kind:9007 -- create the group at its hosts. */
    fun createGroup(engine: NMPEngine, authorPubkeyHex: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(nmpRethrowing { ffi.createGroup(engine.ffi, authorPubkeyHex) })

    /** kind:9008 -- delete the group from its hosts. */
    fun deleteGroup(engine: NMPEngine, authorPubkeyHex: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(nmpRethrowing { ffi.deleteGroup(engine.ffi, authorPubkeyHex) })

    /** kind:9009 -- mint an invite code redeemable by [joinRequest]. */
    fun createInvite(engine: NMPEngine, authorPubkeyHex: String, code: String): NMPGroupWriteStatus =
        NMPGroupWriteStatus.from(
            nmpRethrowing { ffi.createInvite(engine.ffi, authorPubkeyHex, code) },
        )
}

/** A composable NIP-29 discovery predicate (`nmp::nip29::GroupPredicate`/
 * `FfiGroupPredicate` mirror). Opaque by design -- built with
 * [memberListIncludes]/[adminListIncludes] and composed with
 * [union]/[intersect]/[minus], then handed to [NMPRelayScope.groupsWhere]. */
class NMPGroupPredicate private constructor(internal val ffi: FfiGroupPredicate) {
    /** Groups matching this predicate OR any of [others]. */
    fun union(others: List<NMPGroupPredicate>): NMPGroupPredicate =
        NMPGroupPredicate(ffi.union(others.map { it.ffi }))

    /** Groups matching this predicate AND all of [others]. */
    fun intersect(others: List<NMPGroupPredicate>): NMPGroupPredicate =
        NMPGroupPredicate(ffi.intersect(others.map { it.ffi }))

    /** Groups matching this predicate and none of [others]. */
    fun minus(others: List<NMPGroupPredicate>): NMPGroupPredicate =
        NMPGroupPredicate(ffi.minus(others.map { it.ffi }))

    companion object {
        /** Groups whose observed kind:39002 member-list evidence names
         * [subjects]. Inclusion is evidence, never exact state -- absence is
         * not evidence of non-membership. */
        fun memberListIncludes(subjects: NMPBinding): NMPGroupPredicate =
            NMPGroupPredicate(nmpRethrowing { ffiMemberListIncludes(subjects.toFfi()) })

        /** Groups whose observed kind:39001 admin-list evidence names
         * [subjects]. Evidence-scoped exactly like [memberListIncludes]. */
        fun adminListIncludes(subjects: NMPBinding): NMPGroupPredicate =
            NMPGroupPredicate(nmpRethrowing { ffiAdminListIncludes(subjects.toFfi()) })
    }
}

private fun NMPSignedEvent.toFfiSignedEvent(): FfiSignedEvent =
    FfiSignedEvent(id, pubkey, createdAt, kind, tags, content, signature)

/** The ordered [WriteStatus] facts one group write's write reaches, pulled
 * from its untracked receipt handle (#1033). UNLIKE [Receipt] this carries
 * NO receipt id: every [NMPGroup] write reaches the engine's untracked
 * publish door (never `publish_tracked`), because the store-issued
 * receipt-id namespace is a `publish`-door concern the group scope has no
 * reason to surface. The [status] `Flow` is a cold pull loop over `next()`
 * -- FIFO write facts, no folding and no conflation. Collection-scope
 * teardown withdraws the live stream via `handle.cancel()`. */
class NMPGroupWriteStatus private constructor(val status: Flow<WriteStatus>) {
    companion object {
        internal fun from(stream: NmpGroupReceiptStream): NMPGroupWriteStatus =
            NMPGroupWriteStatus(groupWriteStatusFlow(stream))
    }
}

private fun groupWriteStatusFlow(stream: NmpGroupReceiptStream): Flow<WriteStatus> =
    flow {
        try {
            while (true) {
                val status = nmpRethrowingAsync { stream.next() } ?: break
                emit(WriteStatus.from(status))
            }
        } finally {
            stream.cancel()
        }
    }
