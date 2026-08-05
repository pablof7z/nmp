// The NIP-29 door, in ergonomic Kotlin shape (#1033): a relay scope you name
// once, narrowed to a group when you want a specific one. Mirrors
// NIP29.swift.
//
//   val scope = NMPRelayScope.on(listOf("wss://relay-a.example.com"))
//   val mine = NMPGroupIds.memberListIncludes(NMPBinding.Reactive(NMPIdentityField.ActivePubkey))
//   scope.observeRecords(engine, NMPGroupPredicate.naming(mine), listOf(NMPGroupRecord.Metadata))
//       .collect { ... }
//
//   // A directory: every room this relay advertises, 250 per host.
//   scope.observeRecords(engine, NMPGroupPredicate.all(), listOf(NMPGroupRecord.Metadata), 250u)
//       .collect { ... }
//
//   // The room screen: no predicate, no collection, no id lookup.
//   NMPRelayScope.on(listOf(host)).group(roomId)
//       .observeRecords(engine, listOf(NMPGroupRecord.Metadata, NMPGroupRecord.Members))
//       .collect { room -> title = room.metadata?.name ?: roomId }
//
//   val group = scope.group("photographers")                // narrows, contacts nothing
//   val query = group.read(NMPFilter(kinds = listOf(9u)))    // #h-scoped, per-host branches
//   val receipt = group.publish(engine, authorPubkeyHex = pubkeyHex, kind = 9u, content = "hi")
//   receipt.status.collect { ... }
//
//   // Mint now, submit later through your own choke-point (#1242) -- and
//   // stamp your own crash-safe token on the way past (#1244):
//   val intent = group.intent(authorPubkeyHex = pubkeyHex, kind = 9u, content = "hi")
//       .copy(correlation = myPersistedToken)
//   val receipt = publishReceipt(engine.ffi, intent)
//
// `NMPRelayScope`/`NMPGroup`/`NMPGroupPredicate`/`NMPGroupIds` wrap the opaque
// `FfiRelayScope`/`FfiGroup`/`FfiGroupPredicate`/`FfiGroupIds` UniFFI objects exactly like
// `BlossomAuthorization` wraps `FfiBlossomAuthorization` in Blossom.kt -- a
// proven Rust value carried across the boundary, never a second mirrored
// copy of NIP-29's own vocabulary. Neither type exposes its retained hosts
// or group id back out through an accessor; the write door does yield both,
// inside the ordinary `WriteIntent` it mints -- see [NMPGroup.intent].
//
// Deliberately absent, same as before #1033: a fixed group-content kind
// catalog and a kind:9 composer -- NIP-29 owns neither; C7 and client
// notification policy remain independently optional (#838). Also absent:
// any second projection of a NIP-51 Simple-groups entry -- `NIP51.kt` keeps
// that one shape.
//
// Deleted in this change, no alias: the old single-host discovery-demand
// free function pinned to one relay, and `Group`'s single-host constructor.
// A group can live on more than one relay; the single-host door is gone,
// not deprecated.

package com.nmp.sdk

import kotlinx.coroutines.flow.Flow
import kotlinx.coroutines.flow.flow
import uniffi.nmp_ffi.FfiEventBuilder
import uniffi.nmp_ffi.FfiGroup
import uniffi.nmp_ffi.FfiGroupIds
<<<<<<< HEAD
import uniffi.nmp_ffi.FfiGroups
=======
import uniffi.nmp_ffi.FfiGroupMetadataEdit
import uniffi.nmp_ffi.FfiJoinAccess
import uniffi.nmp_ffi.FfiReadAccess
>>>>>>> origin/master
import uniffi.nmp_ffi.FfiGroupPredicate
import uniffi.nmp_ffi.FfiRelayScope
import uniffi.nmp_ffi.FfiSignedEvent
import uniffi.nmp_ffi.NmpEngine
import uniffi.nmp_ffi.FfiGroupAvailability
import uniffi.nmp_ffi.FfiGroupMetadata
import uniffi.nmp_ffi.FfiGroupRecord
import uniffi.nmp_ffi.FfiGroupSnapshot
import uniffi.nmp_ffi.FfiHostRecords
import uniffi.nmp_ffi.FfiListedRecord
import uniffi.nmp_ffi.FfiListedSubject
import uniffi.nmp_ffi.NmpGroupRecordsStream
import uniffi.nmp_ffi.adminListIncludes as ffiAdminListIncludes
import uniffi.nmp_ffi.anyOf as ffiAnyOf
import uniffi.nmp_ffi.groupsWhoseRecordMatches as ffiGroupsWhoseRecordMatches
import uniffi.nmp_ffi.memberListIncludes as ffiMemberListIncludes

/** The relays a NIP-29 group lives on -- named once, retained privately, and
 * never asked for again (`nmp::nip29::RelayScope`/`FfiRelayScope` mirror). */
class NMPRelayScope private constructor(internal val ffi: FfiRelayScope) {
    /** Narrow to one group id, keeping the same hosts. Contacts nothing. */
    fun group(groupId: String): NMPGroup = NMPGroup(ffi.group(groupId))

    /** Narrow to the SEVERAL groups one write belongs to, keeping the same
     * hosts (#1281).
     *
     * The write-only sibling of [group], for the one event shape a single
     * group id cannot express: a kind:30315 session status is addressable at
     * `(author, d=status)` and carries one `h` per room the session occupies,
     * so publishing it once per room would make each copy replace the last.
     * An empty set throws `NMPError.EmptyGroupSet` -- an event with no `h`
     * row is not in a group at all. */
    fun groups(groupIds: List<String>): NMPGroups =
        NMPGroups(nmpRethrowing { ffi.groups(groupIds) })

    /** Watch the relay-signed records of every group matching [predicate].
     * One complete branch per host; each emitted element is the complete set
     * of [NMPGroupSnapshot]s for the groups currently matching. The app never
     * sees a row delta and never walks a `p` tag.
     *
     * [limit] is the ordinary NIP-01 filter limit and bounds EACH host's own
     * branch, never the merged union: two hosts with `250u` may deliver up to
     * 500 snapshots, because each was asked for 250 of its own. `null` asks
     * for whatever the relay chooses to answer with.
     *
     * Each element is the full current state -- latest-wins, never a growing
     * backlog -- so this is a thin pull loop over the Rust-owned handle.
     * Teardown is collection-scope-tied via `handle.cancel()` in a `finally`,
     * identical reasoning to `Query.kt`'s header. */
    fun observeRecords(
        engine: NMPEngine,
        predicate: NMPGroupPredicate,
        records: List<NMPGroupRecord>,
        limit: UInt? = null,
    ): Flow<List<NMPGroupSnapshot>> =
        groupRecordsFlow {
            nmpRethrowing {
                ffi.observeRecords(engine.ffi, predicate.ffi, records.map { it.toFfi() }, limit)
            }
        }

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

    /** Watch THIS group's own relay-signed records. Every emitted element is
     * exactly one [NMPGroupSnapshot] -- this group's -- from the first
     * delivery onward, including before any record has arrived, so there is
     * always something to render.
     *
     * Not a second read door: it drains the Rust-owned handle over the one
     * engine subscription the group's hosts declare. */
    fun observeRecords(
        engine: NMPEngine,
        records: List<NMPGroupRecord>,
    ): Flow<NMPGroupSnapshot> =
        flow {
            val handle = nmpRethrowing { ffi.observeRecords(engine.ffi, records.map { it.toFfi() }) }
            try {
                while (true) {
                    val delivered = nmpRethrowingAsync { handle.next() } ?: break
                    // A group-scoped observation delivers exactly one
                    // snapshot per delivery. A delivery that somehow carried
                    // none is skipped rather than ending the flow: ending it
                    // would tear down a live observation over a delivery that
                    // said nothing.
                    delivered.firstOrNull()?.let { emit(NMPGroupSnapshot.from(it)) }
                }
            } finally {
                handle.cancel()
            }
        }

    /** Ask whether an already-signed event belongs to this group, without
     * building a write out of it. */
    fun validateContext(event: NMPSignedEvent) {
        nmpRethrowing { ffi.validateContext(event.toFfiSignedEvent()) }
    }

    /** Mint the group-contextualized [WriteIntent] for an unsigned draft and
     * publish NOTHING (#1242).
     *
     * This is the write door. The `h` row is appended before signing, the
     * route is the scope's own hosts, and [authorPubkeyHex] is frozen as an
     * exact decoded hex pubkey rather than the active-account selector
     * (#878). Hand the result to `publishReceipt` -- the SAME door every
     * other write takes -- whenever the app's own write path is ready.
     *
     * [WriteIntent.correlation] on the returned intent is `null`, and filling
     * it in is how a group write becomes recoverable after a crash (#1244):
     * an app that persisted its own token before writing copies it in here
     * and finds the write again with the correlation reattach door. */
    fun intent(
        authorPubkeyHex: String,
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        content: String = "",
        createdAt: ULong? = null,
    ): WriteIntent =
        WriteIntent.from(
            nmpRethrowing {
                ffi.intent(authorPubkeyHex, FfiEventBuilder(kind, tags, content, createdAt))
            },
        )

    /** Mint the group-contextualized [WriteIntent] for an ALREADY-SIGNED
     * event, and publish nothing (#1242). The `h` it already carries is
     * validated, never appended or repaired -- see [validateContext]'s doc
     * for the exact refusals, which fire HERE, before any intent exists. */
    fun signedIntent(event: NMPSignedEvent): WriteIntent =
        WriteIntent.from(nmpRethrowing { ffi.signedIntent(event.toFfiSignedEvent()) })

    /** [intent] handed straight to the one publish door -- the inline
     * spelling, for an app with no separate submit stage. Returns the
     * ORDINARY [Receipt], store-issued [Receipt.id] included. */
    fun publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        content: String = "",
        createdAt: ULong? = null,
    ): Receipt =
        receiptFrom(
            nmpRethrowing {
                ffi.publish(
                    engine.ffi,
                    authorPubkeyHex,
                    FfiEventBuilder(kind, tags, content, createdAt),
                )
            },
        )

    /** Publish a draft composed by the tagging door (#1243) into the group.
     *
     * The `h` row and the group's relay set stay this door's, exactly as for
     * the field-by-field overload above: a composer owns the SCHEMA and the
     * group owns the CONTEXT, and neither reaches into the other. What this
     * adds is that a [chatReply] no longer has to be taken apart into
     * kind/tags/content just to be published where it belongs. A pre-signed
     * payload carries its own `h` already and is validated rather than
     * contextualized -- that is [publishSigned]. */
    fun publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        payload: WritePayload,
    ): Receipt =
        when (payload) {
            is WritePayload.Event ->
                publish(
                    engine,
                    authorPubkeyHex,
                    payload.kind,
                    payload.tags,
                    payload.content,
                    payload.createdAt,
                )
            is WritePayload.Signed -> throw NMPError.GroupCallerSuppliedContext
        }

    /** [intent] over a draft the tagging door composed (#1242 + #1243): mint
     * the group's own intent from a composed payload and publish nothing. */
    fun intent(
        authorPubkeyHex: String,
        payload: WritePayload,
    ): WriteIntent =
        when (payload) {
            is WritePayload.Event ->
                intent(
                    authorPubkeyHex,
                    payload.kind,
                    payload.tags,
                    payload.content,
                    payload.createdAt,
                )
            is WritePayload.Signed -> throw NMPError.GroupCallerSuppliedContext
        }

    /** [signedIntent] handed straight to the one publish door. */
    fun publishSigned(engine: NMPEngine, event: NMPSignedEvent): Receipt =
        receiptFrom(nmpRethrowing { ffi.publishSigned(engine.ffi, event.toFfiSignedEvent()) })

    /** kind:9021 -- ask to join. Publishable with no subscription at all. */
    fun joinRequest(
        engine: NMPEngine,
        authorPubkeyHex: String,
        inviteCode: String? = null,
    ): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.joinRequest(engine.ffi, authorPubkeyHex, inviteCode) },
        )

    /** kind:9022 -- leave. */
    fun leaveRequest(engine: NMPEngine, authorPubkeyHex: String): Receipt =
        receiptFrom(nmpRethrowing { ffi.leaveRequest(engine.ffi, authorPubkeyHex) })

    /** kind:9000 -- add a member, optionally with a role. */
    fun addUser(
        engine: NMPEngine,
        authorPubkeyHex: String,
        pubkeyHex: String,
        role: String? = null,
    ): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.addUser(engine.ffi, authorPubkeyHex, pubkeyHex, role) },
        )

    /** kind:9001 -- remove a member. */
    fun removeUser(engine: NMPEngine, authorPubkeyHex: String, pubkeyHex: String): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.removeUser(engine.ffi, authorPubkeyHex, pubkeyHex) },
        )

    /** kind:9002 -- state part of the group's metadata (#1282).
     *
     * Composes NIP-29's own 9002 rows and invents none: `name`, `about` and
     * `picture`, plus the `public`/`private` and `open`/`closed` markers that
     * decide who may read the group and whether join requests are honoured.
     * An omitted field emits no tag, so it is left untouched rather than
     * cleared. */
    fun editMetadata(
        engine: NMPEngine,
        authorPubkeyHex: String,
        edit: NMPGroupMetadataEdit,
    ): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.editMetadata(engine.ffi, authorPubkeyHex, edit.toFfi()) },
        )

    /** kind:9005 -- delete one group-hosted event. */
    fun deleteEvent(engine: NMPEngine, authorPubkeyHex: String, eventId: String): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.deleteEvent(engine.ffi, authorPubkeyHex, eventId) },
        )

    /** kind:9007 -- create the group at its hosts. */
    fun createGroup(engine: NMPEngine, authorPubkeyHex: String): Receipt =
        receiptFrom(nmpRethrowing { ffi.createGroup(engine.ffi, authorPubkeyHex) })

    /** kind:9008 -- delete the group from its hosts. */
    fun deleteGroup(engine: NMPEngine, authorPubkeyHex: String): Receipt =
        receiptFrom(nmpRethrowing { ffi.deleteGroup(engine.ffi, authorPubkeyHex) })

    /** kind:9009 -- mint an invite code redeemable by [joinRequest]. */
    fun createInvite(engine: NMPEngine, authorPubkeyHex: String, code: String): Receipt =
        receiptFrom(
            nmpRethrowing { ffi.createInvite(engine.ffi, authorPubkeyHex, code) },
        )
}

<<<<<<< HEAD
/** The groups one write belongs to (`nmp::nip29::Groups`/`FfiGroups` mirror,
 * #1281), obtained from [NMPRelayScope.groups].
 *
 * A WRITE CONTEXT and nothing else. There is no read door, no records
 * observation and no named operation on it, because each of those is
 * per-group by definition -- a roster is one group's, and every 9000-9022
 * moderation action names one group. A write is the one thing that is
 * genuinely plural.
 *
 * Two methods, both UNSIGNED: NMP appends the `h` rows and NMP signs. There
 * is deliberately no pre-signed spelling and no way to obtain a draft to sign
 * yourself. */
class NMPGroups internal constructor(internal val ffi: FfiGroups) {
    /** Mint the contextualized [WriteIntent] for an unsigned draft and
     * publish NOTHING. [NMPGroup.intent] at a larger arity, not a second
     * door: same appended-before-signing rows, same explicit route over the
     * scope's whole host set, same frozen exact author, same `null`
     * correlation for the caller to stamp. */
    fun intent(
        authorPubkeyHex: String,
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        content: String = "",
        createdAt: ULong? = null,
    ): WriteIntent =
        WriteIntent.from(
            nmpRethrowing {
                ffi.intent(authorPubkeyHex, FfiEventBuilder(kind, tags, content, createdAt))
            },
        )

    /** [intent] handed straight to the one publish door. */
    fun publish(
        engine: NMPEngine,
        authorPubkeyHex: String,
        kind: UShort,
        tags: List<List<String>> = emptyList(),
        content: String = "",
        createdAt: ULong? = null,
    ): Receipt =
        receiptFrom(
            nmpRethrowing {
                ffi.publish(
                    engine.ffi,
                    authorPubkeyHex,
                    FfiEventBuilder(kind, tags, content, createdAt),
                )
            },
        )

=======
/** Who may READ a group's messages (`nmp::nip29::ReadAccess` mirror, #1282).
 *
 * NIP-29 spells the restricted state `["private"]`; the reference relay's
 * kind:9002 parser spells the permissive one `["public"]`, which is the only
 * way an edit can say "turn it back off". */
enum class NMPReadAccess {
    /** `["public"]` -- anyone may read the group's messages. */
    PUBLIC,

    /** `["private"]` -- only members may read the group's messages. */
    PRIVATE,
    ;

    internal fun toFfi(): FfiReadAccess =
        when (this) {
            PUBLIC -> FfiReadAccess.PUBLIC
            PRIVATE -> FfiReadAccess.PRIVATE
        }
}

/** Whether JOIN REQUESTS are honoured (`nmp::nip29::JoinAccess` mirror,
 * #1282). Independent of [NMPReadAccess]: a group can be publicly readable
 * and still closed to new members. */
enum class NMPJoinAccess {
    /** `["open"]` -- join requests are honoured. */
    OPEN,

    /** `["closed"]` -- join requests are ignored. */
    CLOSED,
    ;

    internal fun toFfi(): FfiJoinAccess =
        when (this) {
            OPEN -> FfiJoinAccess.OPEN
            CLOSED -> FfiJoinAccess.CLOSED
        }
}

/** What one kind:9002 edit says about a group
 * (`nmp::nip29::GroupMetadataEdit` mirror, #1282).
 *
 * Every field is optional: `null` leaves that row out of the draft entirely,
 * so it is not touched and never cleared. That is why the two markers are
 * two-valued enums rather than `Boolean`s -- "make it public" and "do not
 * decide" are different statements, and one `Boolean` cannot make both. */
data class NMPGroupMetadataEdit(
    /** The `name` row -- the group's display name. */
    val name: String? = null,
    /** The `about` row -- the group's description. */
    val about: String? = null,
    /** The `picture` row. The tag NAME is NIP-29's; which URL goes in it is
     * entirely the app's product policy. */
    val picture: String? = null,
    /** Who may read the group's messages. */
    val readAccess: NMPReadAccess? = null,
    /** Whether join requests are honoured. */
    val joinAccess: NMPJoinAccess? = null,
) {
    internal fun toFfi(): FfiGroupMetadataEdit =
        FfiGroupMetadataEdit(name, about, picture, readAccess?.toFfi(), joinAccess?.toFfi())
>>>>>>> origin/master
}

/** Which groups an observation covers (`nmp::nip29::GroupPredicate`/
 * `FfiGroupPredicate` mirror). Opaque by design -- built with [all] or
 * [naming], then handed to [NMPRelayScope.observeRecords].
 *
 * Set algebra lives on [NMPGroupIds] and on nothing else, so
 * `all().minus(...)` does not compile. Nostr filters have no negation, so
 * "everything except X" cannot narrow a wire request; an app that hides
 * muted rooms drops them from the snapshots it renders, where the cost is
 * visible. */
class NMPGroupPredicate private constructor(internal val ffi: FfiGroupPredicate) {
    companion object {
        /** Every group the host advertises among the selected records. The
         * branch carries NO group-id row: this is the ABSENCE of a
         * constraint, which is what makes a directory expressible -- the ids
         * a directory wants are the answer, not the input.
         *
         * Unbounded by nature: bound it with [NMPRelayScope.observeRecords]'s
         * own `limit`. Advertisement is not enumeration -- a group the host
         * serves but publishes no kind:39000 for is invisible. */
        fun all(): NMPGroupPredicate = NMPGroupPredicate(FfiGroupPredicate.all())

        /** Only the groups [ids] names. */
        fun naming(ids: NMPGroupIds): NMPGroupPredicate =
            NMPGroupPredicate(FfiGroupPredicate.naming(ids.ffi))
    }
}

/** Where a set of NIP-29 group ids comes from (`nmp::nip29::GroupIds`/
 * `FfiGroupIds` mirror). Opaque by design -- built with
 * [memberListIncludes]/[adminListIncludes]/[anyOf]/[whoseRecordMatches] and
 * composed with [union]/[intersect]/[minus].
 *
 * Whatever this resolves to becomes the `#d` value set of one relay filter,
 * and a filter carrying very many values may be refused or silently
 * truncated by that relay. Watching very many groups needs sharding across
 * several observations; NMP does not chunk behind the app's back. */
class NMPGroupIds private constructor(internal val ffi: FfiGroupIds) {
    /** Groups named by this source OR by any of [others]. */
    fun union(others: List<NMPGroupIds>): NMPGroupIds =
        NMPGroupIds(ffi.union(others.map { it.ffi }))

    /** Groups named by this source AND by all of [others]. */
    fun intersect(others: List<NMPGroupIds>): NMPGroupIds =
        NMPGroupIds(ffi.intersect(others.map { it.ffi }))

    /** Groups named by this source and by none of [others]. */
    fun minus(others: List<NMPGroupIds>): NMPGroupIds =
        NMPGroupIds(ffi.minus(others.map { it.ffi }))

    companion object {
        /** Groups whose own relay-signed record matches [selection] at the
         * branch host -- THE general spelling, of which every leaf below is a
         * shorthand. Throws when [selection] names no kind, or names a kind
         * that is not one of NIP-29's three relay-signed group records: this
         * leaf is evaluated with NIP-29's own pin, and a group host is
         * authoritative for nothing else. */
        fun whoseRecordMatches(selection: NMPFilter): NMPGroupIds =
            NMPGroupIds(nmpRethrowing { ffiGroupsWhoseRecordMatches(selection.toFfi()) })

        /** Groups whose observed kind:39002 member-list evidence names
         * [subjects]. Inclusion is evidence, never exact state -- absence is
         * not evidence of non-membership.
         *
         * Shorthand for `whoseRecordMatches({ kinds:[39002], #p: subjects })`. */
        fun memberListIncludes(subjects: NMPBinding): NMPGroupIds =
            NMPGroupIds(nmpRethrowing { ffiMemberListIncludes(subjects.toFfi()) })

        /** Groups whose observed kind:39001 admin-list evidence names
         * [subjects]. Evidence-scoped exactly like [memberListIncludes]. */
        fun adminListIncludes(subjects: NMPBinding): NMPGroupIds =
            NMPGroupIds(nmpRethrowing { ffiAdminListIncludes(subjects.toFfi()) })

        /** The groups [ids] names, whatever any list says about them.
         *
         * [ids] is an ordinary [NMPBinding]: a literal set for rooms the app
         * already knows, and a derived binding for rooms it has to look up.
         * "Watch the groups named in my own kind:10009 simple-groups list" is
         * that derived case, and it stays reactive -- when the list changes,
         * the observation follows it. A derived binding keeps its OWN
         * authority and is never repinned to the group's hosts. */
        fun anyOf(ids: NMPBinding): NMPGroupIds =
            NMPGroupIds(nmpRethrowing { ffiAnyOf(ids.toFfi()) })
    }
}

private fun NMPSignedEvent.toFfiSignedEvent(): FfiSignedEvent =
    FfiSignedEvent(id, pubkey, createdAt, kind, tags, content, signature)

// ===========================================================================
// The relay-signed group records (#1233).
//
// Every value below is copied straight out of Rust. No `p`-tag walking, no
// role defaulting, no cross-host merge policy lives on this side of the FFI
// boundary -- this file only mirrors Rust-owned state and drains Rust-owned
// handles, exactly like `Following.kt` does for NIP-02.
// ===========================================================================

/** What a group's hosts currently PROVE about one subject's place in one of
 * NIP-29's relay-signed lists ([uniffi.nmp_ffi.FfiListing] mirror, #1234).
 *
 * Three cases because NIP-29 admits three. Collapsing the last two is the
 * mistake: an app that renders [Unestablished] as "not a member" shows a
 * false negative every time a relay is still reconciling. */
sealed class NMPListing {
    /** At least one host's own signed record names this subject, with the
     * hosts that named it and the role each of them wrote. */
    data class Named(val entries: List<NMPListedSubject>) : NMPListing()

    /** Every host in the scope has ESTABLISHED this record and none names the
     * subject. Rests on settlement, never on a timer. */
    data object Absent : NMPListing()

    /** Not every host has established this record yet, or the observation
     * never asked for it. Absence is not evidence here. */
    data object Unestablished : NMPListing()

    companion object {
        internal fun from(ffi: uniffi.nmp_ffi.FfiListing): NMPListing =
            when (ffi) {
                is uniffi.nmp_ffi.FfiListing.Named ->
                    Named(ffi.entries.map { NMPListedSubject.from(it) })
                is uniffi.nmp_ffi.FfiListing.Absent -> Absent
                is uniffi.nmp_ffi.FfiListing.Unestablished -> Unestablished
            }
    }
}

/** Which of NIP-29's three relay-signed group records you are asking for
 * (`FfiGroupRecord` mirror). */
enum class NMPGroupRecord {
    /** kind:39000 -- the group's own metadata. */
    Metadata,

    /** kind:39001 -- the optional, informative admin list. */
    Admins,

    /** kind:39002 -- the optional, possibly partial member list. */
    Members,
    ;

    internal fun toFfi(): FfiGroupRecord =
        when (this) {
            Metadata -> FfiGroupRecord.METADATA
            Admins -> FfiGroupRecord.ADMINS
            Members -> FfiGroupRecord.MEMBERS
        }

    companion object {
        fun from(ffi: FfiGroupRecord): NMPGroupRecord =
            when (ffi) {
                FfiGroupRecord.METADATA -> Metadata
                FfiGroupRecord.ADMINS -> Admins
                FfiGroupRecord.MEMBERS -> Members
            }
    }
}

/** How much of what you asked for has been established (`FfiGroupAvailability`
 * mirror). It says nothing about whether the records are complete: a relay
 * that is [Ready] and published no member list has published no member list. */
enum class NMPGroupAvailability {
    SourceUnavailable,
    Acquiring,
    CachedOnly,
    Ready,
    ;

    companion object {
        fun from(ffi: FfiGroupAvailability): NMPGroupAvailability =
            when (ffi) {
                FfiGroupAvailability.SOURCE_UNAVAILABLE -> SourceUnavailable
                FfiGroupAvailability.ACQUIRING -> Acquiring
                FfiGroupAvailability.CACHED_ONLY -> CachedOnly
                FfiGroupAvailability.READY -> Ready
            }
    }
}

/** One subject a relay-signed list names, and the hosts that named it
 * (`FfiListedSubject` mirror). [role] is null when the relay wrote none --
 * never defaulted to "member". */
data class NMPListedSubject(
    val pubkey: String,
    val role: String?,
    val hosts: List<String>,
) {
    companion object {
        fun from(ffi: FfiListedSubject): NMPListedSubject =
            NMPListedSubject(ffi.pubkey, ffi.role, ffi.hosts)
    }
}

/** One relay-signed list record (`FfiListedRecord` mirror). */
data class NMPListedRecord(
    val subjects: List<NMPListedSubject>,
    /** The record's own `created_at`. A DISPLAY fact about this relay's
     * record -- never compared against a local clock to adjudicate anything. */
    val asOf: ULong,
    val eventId: String,
    val host: String,
) {
    companion object {
        fun from(ffi: FfiListedRecord): NMPListedRecord =
            NMPListedRecord(
                subjects = ffi.subjects.map { NMPListedSubject.from(it) },
                asOf = ffi.asOf,
                eventId = ffi.eventId,
                host = ffi.host,
            )
    }
}

/** One relay-signed kind:39000 record (`FfiGroupMetadata` mirror). The three
 * rows NIP-29 names are typed; [tags] carries the record's complete row list
 * verbatim, so reading a row NIP-29 core does not define (a `parent`, say)
 * needs no hand-parser here. */
data class NMPGroupMetadata(
    val name: String?,
    val about: String?,
    val picture: String?,
    val tags: List<List<String>>,
    val asOf: ULong,
    val eventId: String,
    /** The relay that signed this record. */
    val host: String,
) {
    companion object {
        fun from(ffi: FfiGroupMetadata): NMPGroupMetadata =
            NMPGroupMetadata(
                name = ffi.name,
                about = ffi.about,
                picture = ffi.picture,
                tags = ffi.tags,
                asOf = ffi.asOf,
                eventId = ffi.eventId,
                host = ffi.host,
            )
    }
}

/** Exactly what one host signed, folded with nothing (`FfiHostRecords`
 * mirror). Each record is nullable because a relay genuinely may publish one,
 * two, or none of the three -- null means "this host has published none we
 * have seen", never "there are none". */
data class NMPHostRecords(
    val host: String,
    val metadata: NMPGroupMetadata?,
    val admins: NMPListedRecord?,
    val members: NMPListedRecord?,
    val availability: NMPGroupAvailability,
) {
    companion object {
        fun from(ffi: FfiHostRecords): NMPHostRecords =
            NMPHostRecords(
                host = ffi.host,
                metadata = ffi.metadata?.let { NMPGroupMetadata.from(it) },
                admins = ffi.admins?.let { NMPListedRecord.from(it) },
                members = ffi.members?.let { NMPListedRecord.from(it) },
                availability = NMPGroupAvailability.from(ffi.availability),
            )
    }
}

/** One group, as the hosts currently describe it (`FfiGroupSnapshot` mirror).
 * A complete self-contained value, never a patch on a previous one. */
data class NMPGroupSnapshot(
    /** The `d` value the relay-signed records key themselves by. */
    val id: String,
    /** The whole winning host's record -- latest `created_at` wins, never a
     * field-wise merge. [NMPGroupMetadata.host] says which relay signed it. */
    val metadata: NMPGroupMetadata?,
    /** The union across hosts, each entry carrying the hosts that named it. */
    val admins: List<NMPListedSubject>,
    val members: List<NMPListedSubject>,
    /** The minimum over every host in the scope. */
    val availability: NMPGroupAvailability,
    /** Exactly what each host that answered signed, in host order. */
    val perHost: List<NMPHostRecords>,
    /** The records the answering hosts do not agree on. */
    val disagreements: Set<NMPGroupRecord>,
    /** The records this observation ASKED for. Settlement alone cannot tell a
     * settled empty list apart from a list nobody requested, and [NMPListing.Absent]
     * would be a lie in the second case (#1234). */
    val selected: Set<NMPGroupRecord>,
    /** Retained verbatim so [memberListing]/[adminListing] ask Rust the
     * question instead of re-deriving the answer here. The same record this
     * value was built from. */
    private val ffi: FfiGroupSnapshot,
) {
    /** Exactly what [host] signed, or null if it has published none of the
     * selected records for this group that we have seen. */
    fun at(host: String): NMPHostRecords? = perHost.firstOrNull { it.host == host }

    /** Whether the hosts disagree about [record], so a UI can decide whether
     * a dig-in affordance is worth offering. */
    fun differs(record: NMPGroupRecord): Boolean = disagreements.contains(record)

    /** What these hosts PROVE about [subjectPubkeyHex]'s place in their
     * kind:39002 member lists (#1234).
     *
     * This is the answer a moderation receipt cannot give: an acked kind:9000
     * says a host took the request, and only this says the list reflects it.
     * [subjectPubkeyHex] is 64-char hex; a malformed one throws rather than
     * reading as a settled absence. */
    fun memberListing(subjectPubkeyHex: String): NMPListing =
        NMPListing.from(nmpRethrowing { uniffi.nmp_ffi.memberListing(ffi, subjectPubkeyHex) })

    /** The kind:39001 half. Same rule as [memberListing]. */
    fun adminListing(subjectPubkeyHex: String): NMPListing =
        NMPListing.from(nmpRethrowing { uniffi.nmp_ffi.adminListing(ffi, subjectPubkeyHex) })

    companion object {
        fun from(ffi: FfiGroupSnapshot): NMPGroupSnapshot =
            NMPGroupSnapshot(
                id = ffi.id,
                metadata = ffi.metadata?.let { NMPGroupMetadata.from(it) },
                admins = ffi.admins.map { NMPListedSubject.from(it) },
                members = ffi.members.map { NMPListedSubject.from(it) },
                availability = NMPGroupAvailability.from(ffi.availability),
                perHost = ffi.perHost.map { NMPHostRecords.from(it) },
                disagreements = ffi.disagreements.map { NMPGroupRecord.from(it) }.toSet(),
                selected = ffi.selected.map { NMPGroupRecord.from(it) }.toSet(),
                ffi = ffi,
            )
    }
}

/** Shared pull loop for a scope-wide group-records observation. Each element
 * is the complete current snapshot set (latest-wins, conflated by the
 * engine's own mailbox), so this folds nothing of its own. */
private fun groupRecordsFlow(open: () -> NmpGroupRecordsStream): Flow<List<NMPGroupSnapshot>> =
    flow {
        val handle = open()
        try {
            while (true) {
                val delivered = nmpRethrowingAsync { handle.next() } ?: break
                emit(delivered.map { NMPGroupSnapshot.from(it) })
            }
        } finally {
            handle.cancel()
        }
    }
