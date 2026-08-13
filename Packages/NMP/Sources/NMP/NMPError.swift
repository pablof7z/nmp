// The ergonomic error surface: `NMP`'s public API never leaks the
// `Ffi*`-prefixed generated types (M4 plan §2/§9 -- "hide the Ffi* types
// behind ergonomic Swift enums/structs"). `FfiError` already conforms to
// `Swift.Error`, but re-wrapping it keeps the public surface entirely free
// of the `Ffi` prefix, matching every other value type in this package.

import Foundation
import NMPFFI

/// Every way a call into the engine can fail -- typed states, never a crash
/// (mirrors `nmp-ffi`'s own `FfiError`; see that type's doc for the Rust
/// side of each case).
///
/// NOTE: there is deliberately no `.invalidSignedEvent` case anymore -- a
/// `WritePayload.signed` event that fails `nostr::Event::verify` is rejected
/// at `nmp-engine`'s acceptance boundary rather than here, so the guarantee
/// holds for every entry point rather than only this one -- and because that
/// instruction can never resolve, it surfaces as `.publishRefused` from
/// `publish` itself rather than as a fact on a receipt stream nothing will
/// ever add to.
/// Receipt-correlation exhaustion is synchronous because no truthful
/// `Receipt` or status stream can be created without an identity.
///
public enum NMPError: Error, Sendable, Equatable {
    case nonIndexableFilterTag(String)
    case invalidPublicKey(String)
    case invalidEventId(String)
    case invalidRelayUrl(String)
    // nmp-native:if nip65
    case outboxRoutingIndexersEmpty
    case automaticRoutingUnavailable
    // nmp-native:endif
    case invalidTag([String])
    case invalidSecretKey
    case invalidSigner(String)
    case authCapabilityRegistryFull(limit: UInt64)
    case authCapabilityInstanceExhausted
    case noActiveSigner
    case invalidSignRequest(String)
    case signerUnavailable(String)
    case signerRejected(String)
    case invalidSignerOutput(String)
    /// `publish` refused the call outright: either NMP could not write
    /// anything down, or the instruction could not resolve (no active
    /// account, a signature that does not verify, an explicit identity
    /// contradicting a signed payload's author, a reserved kind, an empty
    /// explicit route). Nothing durable exists and there is no queue entry to
    /// inspect.
    ///
    /// Everything else takes CUSTODY and fails in the queue where you can see
    /// it -- including a stale replaceable base, which succeeds here and
    /// arrives as `WriteOutcome.refused`.
    case publishRefused(String)
    /// `NmpEngineConfig.storePath` pointed at a file the on-disk store could
    /// not open: damaged bytes, a refused lock, an unresolvable path, an I/O
    /// failure.
    ///
    /// A positive claim, not a catch-all (#920): **deleting the store is not
    /// the recovery for this.** The one refusal a fresh store does fix is
    /// `storeUnsupportedSchema`, and it never arrives here.
    case storeOpenFailed(String)
    /// #489: `NmpEngineConfig.storePath` names a persistent store already
    /// owned by this or another process. No second database owner and no
    /// partial engine were created.
    case storeAlreadyOpen(String)
    /// #867/#920: `NmpEngineConfig.storePath` holds durable bytes that are
    /// not the one schema epoch this build supports. Nothing was migrated,
    /// adopted, drained, or reset, and no engine was constructed.
    ///
    /// The only response that lets this build run is to close every owner,
    /// delete the store, and create a fresh one -- your call, through the
    /// separate destructive reset. The relay-backed read cache is
    /// reacquirable; the publish queue is not, so accepted but unpublished
    /// writes and their receipts, correlation tokens, route revisions, and
    /// attempt evidence go with it.
    ///
    /// `found` is `nil` when the store carries no marker this build can
    /// read, which includes a marker written at an address a superseded
    /// epoch owned. `nil` means "not this epoch", never "no data".
    case storeUnsupportedSchema(path: String, expected: UInt64, found: UInt64?)
    case storeResetFailed(String)
    case storeStillOpen(String)
    /// The engine could not be constructed (`NmpEngine.init`): a genuine
    /// engine-start infrastructure failure. Never raised by an ordinary
    /// operation (#704).
    case engineStartFailed(component: String, reason: String)
    /// A windowed `observe` could not open its canonical history projection
    /// because the store degraded during setup. This is the case's sole
    /// production meaning; relay connection/worker failure remains ordinary
    /// acquisition evidence in the observation stream (#704).
    case observationUnavailable(reason: String)
    /// #680: a second `next()` was awaited on an observation stream (or a
    /// `signed()` on a sign handle) while a previous one was still in flight.
    /// Every observation handle is single-consumer -- surface the misuse as a
    /// typed error, never a hang. No frame is lost or duplicated.
    case concurrentNext
    /// A durable FIFO fact stream crossed its finite live-delivery bound
    /// while the app was paused. No fact is claimed delivered and memory stays
    /// bounded; when present, reattach `receiptId` to replay persisted facts.
    case factStreamLagged(receiptId: UInt64?)
    case receiptReplayUnavailable(receiptId: UInt64)
    case receiptClosedWithoutOutcome(receiptId: UInt64)
    /// #680: `NmpSignEventHandle.signed()` was awaited a second time -- the
    /// one-shot result was already delivered to the first await.
    case signEventAlreadyConsumed
    case invalidSignature(String)
    case engineClosed
    /// `decodeNostrEntity`'s input was not valid bech32, had an
    /// unrecognized HRP prefix, or had a malformed inner TLV payload
    /// (#116).
    case invalidNostrEntity(String)
    /// `decodeNostrEntity`'s input decoded to `nsec`/`ncryptsec` -- refused
    /// rather than decoded (#116).
    case nostrEntitySecretKeyRejected
    /// An `NMPDemand` declared `.authorOutboxes` over a selection whose
    /// `authors` field is unbound (#107).
    case authorOutboxesRequiresBoundAuthors
    /// An `NMPDemand` declared `.pinned([])` -- an empty relay set (#107
    /// Contract: "the pinned relay set must be nonempty").
    case emptyPinnedRelaySet
    /// A windowed `observe` declared a zero `initial` or `max` row count
    /// (#485) -- an empty window can neither deliver nor grow.
    case windowZeroRows
    /// A windowed `observe` declared `initial > max` (#485).
    case windowInitialExceedsMax(initial: UInt64, max: UInt64)
    /// A windowed `observe` was given a selection that already carries its
    /// own NIP-01 `limit` (#485) -- the window IS the bound; carrying a
    /// second, competing bound on the wire filter is refused rather than
    /// silently reconciled.
    case windowSelectionHasLimit
    /// A windowed `observe` was given a live query that already declares an
    /// aggregate result limit (#1108) -- the window and the aggregate bound
    /// would be two competing owners of the merged row count.
    case windowAggregateResultLimit
    /// A live query was declared with no demand branches at all (#1108).
    case emptyQueryUnion
    /// A live query declared an aggregate result limit of zero (#1108): a
    /// query that may never contain a row is not a bound.
    case aggregateResultLimitZero
    /// A nested live-query branch carried its own aggregate result limit
    /// (#1108). Branches flatten into one canonical set, so an inner bound
    /// has no surviving scope and accepting it would silently discard it.
    case nestedAggregateResultLimit
    /// A live query declared more branches than the supported hard ceiling
    /// (#1108). The whole declaration is refused; no subset is installed.
    case tooManyQueryBranches(requested: UInt64, maximum: UInt64)
    /// No last-good NIP-11 document exists and acquisition failed.
    case relayInformationUnavailable(RelayInformationErrorKind)
    /// #591: `WriteIntent.correlation`/`reattachReceipt(correlation:)` was
    /// given a token that failed `CorrelationToken`'s bounded/non-empty
    /// validation.
    case invalidCorrelationToken(got: String, reason: String)
    // nmp-native:if nip22
    /// #572/#1258: an `Nip73` failed its constructor validation (an empty
    /// `I`/`K` cell).
    case invalidNip73(reason: String)
    // nmp-native:endif
    // nmp-native:if nip25
    /// #155: a `Reaction.emoji` said something the caller did not mean --
    /// the empty string, which NIP-25 reads as a like, or a NIP-30
    /// `:shortcode:`, which needs a companion `emoji` row this door does not
    /// write.
    case invalidReaction(reason: String)
    // nmp-native:endif
    // nmp-native:if nip22
    /// #973: a composer returned a compare-and-swap replaceable edit, which
    /// has no wire form on purpose -- a replaceable precondition crosses
    /// this boundary only inside the semantic method that owns it
    /// (`follow`/`unfollow`), never as a payload a native caller could
    /// reassemble without the guard.
    case replaceableEditHasNoWireForm
    // nmp-native:endif
    // nmp-native:if nip29
    /// #1033: `NMPRelayScope.on`/`FfiRelayScope.on` was given an empty
    /// relay set -- a group must be hosted somewhere.
    case emptyRelayScope
    /// #1033: an event builder handed to `NMPGroup.publish` already carried
    /// its own `h` tag. The retained group id is the sole semantic source of
    /// that tag; a caller-supplied one is refused before any write reaches
    /// the door.
    case groupCallerSuppliedContext
    /// #1033: a read selection handed to `NMPGroup.read` already constrained
    /// `#h`. The retained group id is the sole semantic source of that row.
    case groupCallerSuppliedContextConstraint
    /// #1033: a read selection handed to `NMPGroup.read` already declared a
    /// `since`/`until`/`limit` timeline bound the group door itself owns.
    case groupCallerSuppliedTimeline
    /// #1281: `NMPRelayScope.groups(_:)` was given no group id at all. An
    /// event with no `h` row is not in a group, so there is nothing to
    /// contextualize and no honest route to mint.
    case emptyGroupSet
    /// #1033/#1281: `NMPGroup.validateContext` was given an event carrying
    /// no `h` tag naming any group at all. `expected` is the
    /// whole set the door was asked for -- one id for an `NMPGroup`, several
    /// for an `NMPGroups`.
    case groupContextMissing(expected: [String])
    /// #1033/#1281: an already-signed event's `h` tags name a different SET
    /// of groups than the one it was handed to -- too few, too many, or the
    /// wrong ones.
    case groupContextMismatched(found: [String], expected: [String])
    /// #1281: an already-signed event names the right groups but repeats one
    /// of them in a second `h` row, which is not a row the door would mint.
    case groupContextRepeated(repeated: [String])
    /// #1245: a group content read named one of NIP-29's own relay-signed
    /// group records. Those identify themselves a different way, so an
    /// `h`-scoped read of them can only ever match nothing -- read them
    /// through `NMPGroup.observeRecords(engine:records:)` instead.
    case groupRecordsNotContextScoped(kinds: [UInt16])
    /// #1233: a records observation named none of the three records, which
    /// would deliver a permanently empty state.
    case groupNoRecordSelected
    /// A kind:9000 or kind:9001 operation named no users.
    case groupUserBatchEmpty
    /// One kind:9000 operation assigned one user conflicting roles.
    case groupUserBatchConflictingRoles(pubkey: String)
    /// #1252: a selection handed to `NMPGroupIds.whoseRecordMatches(_:)`
    /// named no kind. It is evaluated with NIP-29's own pin, so it would
    /// match every event the group's host holds.
    case groupIdSelectionNamesNoKind
    /// #1252: a selection handed to `NMPGroupIds.whoseRecordMatches(_:)`
    /// named a kind that is not one of NIP-29's three relay-signed group
    /// records. That leaf is evaluated at the group's host, which is not
    /// authoritative for anything else, so the read would silently
    /// under-resolve. Ids that come from the app's OWN data go through
    /// `NMPGroupIds.anyOf(_:)` as a derived binding carrying its own
    /// authority.
    case groupIdSelectionNotAGroupRecordKind(kind: UInt16)
    // nmp-native:endif

    init(_ ffi: FfiError) {
        switch ffi {
        case .NonIndexableFilterTag(let got): self = .nonIndexableFilterTag(got)
        case .InvalidPublicKey(let got): self = .invalidPublicKey(got)
        case .InvalidEventId(let got): self = .invalidEventId(got)
        case .InvalidRelayUrl(let got): self = .invalidRelayUrl(got)
        // nmp-native:if nip65
        case .OutboxRoutingIndexersEmpty: self = .outboxRoutingIndexersEmpty
        case .AutomaticRoutingUnavailable: self = .automaticRoutingUnavailable
        // nmp-native:endif
        case .InvalidTag(let got): self = .invalidTag(got)
        case .InvalidSecretKey: self = .invalidSecretKey
        case .InvalidSigner(let reason): self = .invalidSigner(reason)
        case .AuthCapabilityRegistryFull(let limit):
            self = .authCapabilityRegistryFull(limit: limit)
        case .AuthCapabilityInstanceExhausted:
            self = .authCapabilityInstanceExhausted
        case .NoActiveSigner: self = .noActiveSigner
        case .InvalidSignRequest(let reason): self = .invalidSignRequest(reason)
        case .PublishRefused(let reason): self = .publishRefused(reason)
        case .StoreOpenFailed(let reason): self = .storeOpenFailed(reason)
        case .StoreAlreadyOpen(let path): self = .storeAlreadyOpen(path)
        case .StoreUnsupportedSchema(let path, let expected, let found):
            self = .storeUnsupportedSchema(path: path, expected: expected, found: found)
        case .StoreResetFailed(let reason): self = .storeResetFailed(reason)
        case .StoreStillOpen(let path): self = .storeStillOpen(path)
        case .EngineStartFailed(let component, let reason):
            self = .engineStartFailed(component: component, reason: reason)
        case .ObservationUnavailable(let reason):
            self = .observationUnavailable(reason: reason)
        case .ConcurrentNext: self = .concurrentNext
        case .FactStreamLagged(let receiptId):
            self = .factStreamLagged(receiptId: receiptId)
        case .ReceiptReplayUnavailable(let receiptId):
            self = .receiptReplayUnavailable(receiptId: receiptId)
        case .ReceiptClosedWithoutOutcome(let receiptId):
            self = .receiptClosedWithoutOutcome(receiptId: receiptId)
        case .InvalidSignature(let got): self = .invalidSignature(got)
        case .EngineClosed: self = .engineClosed
        case .InvalidNostrEntity(let reason): self = .invalidNostrEntity(reason)
        case .NostrEntitySecretKeyRejected: self = .nostrEntitySecretKeyRejected
        case .AuthorOutboxesRequiresBoundAuthors: self = .authorOutboxesRequiresBoundAuthors
        case .EmptyPinnedRelaySet: self = .emptyPinnedRelaySet
        case .WindowZeroRows: self = .windowZeroRows
        case .WindowInitialExceedsMax(let initial, let max):
            self = .windowInitialExceedsMax(initial: initial, max: max)
        case .WindowSelectionHasLimit: self = .windowSelectionHasLimit
        case .WindowAggregateResultLimit: self = .windowAggregateResultLimit
        case .EmptyQueryUnion: self = .emptyQueryUnion
        case .AggregateResultLimitZero: self = .aggregateResultLimitZero
        case .NestedAggregateResultLimit: self = .nestedAggregateResultLimit
        case .TooManyQueryBranches(let requested, let maximum):
            self = .tooManyQueryBranches(requested: requested, maximum: maximum)
        case .RelayInformationUnavailable(let kind):
            self = .relayInformationUnavailable(RelayInformationErrorKind(kind))
        case .InvalidCorrelationToken(let got, let reason):
            self = .invalidCorrelationToken(got: got, reason: reason)
        // nmp-native:if nip22
        case .InvalidNip73(let reason):
            self = .invalidNip73(reason: reason)
        // nmp-native:endif
        // nmp-native:if nip25
        case .InvalidReaction(let reason):
            self = .invalidReaction(reason: reason)
        // nmp-native:endif
        // nmp-native:if nip22
        case .ReplaceableEditHasNoWireForm: self = .replaceableEditHasNoWireForm
        // nmp-native:endif
        // nmp-native:if nip29
        case .EmptyRelayScope: self = .emptyRelayScope
        case .GroupCallerSuppliedContext: self = .groupCallerSuppliedContext
        case .GroupCallerSuppliedContextConstraint:
            self = .groupCallerSuppliedContextConstraint
        case .GroupCallerSuppliedTimeline: self = .groupCallerSuppliedTimeline
        case .EmptyGroupSet: self = .emptyGroupSet
        case .GroupContextMissing(let expected): self = .groupContextMissing(expected: expected)
        case .GroupContextMismatched(let found, let expected):
            self = .groupContextMismatched(found: found, expected: expected)
        case .GroupContextRepeated(let repeated):
            self = .groupContextRepeated(repeated: repeated)
        case .GroupRecordsNotContextScoped(let kinds):
            self = .groupRecordsNotContextScoped(kinds: kinds)
        case .GroupNoRecordSelected:
            self = .groupNoRecordSelected
        case .GroupUserBatchEmpty:
            self = .groupUserBatchEmpty
        case .GroupUserBatchConflictingRoles(let pubkey):
            self = .groupUserBatchConflictingRoles(pubkey: pubkey)
        case .GroupIdSelectionNamesNoKind:
            self = .groupIdSelectionNamesNoKind
        case .GroupIdSelectionNotAGroupRecordKind(let kind):
            self = .groupIdSelectionNotAGroupRecordKind(kind: kind)
        // nmp-native:endif
        }
    }
}

extension NMPError: LocalizedError {
    /// Stable native presentation for every typed failure. Keep this switch
    /// exhaustive: adding a new error case must also decide what ordinary
    /// Swift clients show without discarding its evidence.
    public var errorDescription: String? {
        switch self {
        case .nonIndexableFilterTag(let got):
            "Not indexable as a filter key: \(got.debugDescription)"
        case .invalidPublicKey(let got):
            "Invalid public key hex: \(got.debugDescription)"
        case .invalidEventId(let got):
            "Invalid event ID hex: \(got.debugDescription)"
        case .invalidRelayUrl(let got):
            "Invalid relay URL: \(got.debugDescription)"
        // nmp-native:if nip65
        case .outboxRoutingIndexersEmpty:
            "Outbox routing requires at least one app-owned indexer"
        case .automaticRoutingUnavailable:
            "Automatic routing is unavailable; configure outbox routing indexers"
        // nmp-native:endif
        case .invalidTag(let got):
            "Invalid tag: \(String(reflecting: got))"
        case .invalidSecretKey:
            "Invalid secret key"
        case .invalidSigner(let reason):
            "Invalid signer: \(reason)"
        case .authCapabilityRegistryFull(let limit):
            "AUTH capability registry is full at \(limit) entries"
        case .authCapabilityInstanceExhausted:
            "AUTH capability instance space exhausted"
        case .noActiveSigner:
            "The active account has no registered signer"
        case .invalidSignRequest(let reason):
            "Invalid sign request: \(reason)"
        case .signerUnavailable(let reason):
            "Signer unavailable: \(reason)"
        case .signerRejected(let reason):
            "Signer rejected the request: \(reason)"
        case .invalidSignerOutput(let reason):
            "Invalid signer output: \(reason)"
        case .publishRefused(let reason):
            reason
        case .storeOpenFailed(let reason):
            "Could not open store: \(reason)"
        case .storeAlreadyOpen(let path):
            "Persistent store is already open: \(path)"
        case .storeUnsupportedSchema(let path, let expected, let found):
            (found.map {
                "Persistent store \(path) is schema epoch \($0), not the one supported epoch \(expected)"
            } ?? "Persistent store \(path) carries no readable schema marker and is not the one supported epoch \(expected)")
                + "; it was not migrated, adopted, drained, or reset; discard and recreate this store to continue;"
                + " NMP can reacquire the relay-backed read cache, but the publish queue state (accepted but"
                + " unpublished writes, receipts, correlation tokens, route revisions, and attempt evidence) will be"
                + " permanently lost"
        case .storeResetFailed(let reason):
            "Could not reset store: \(reason)"
        case .storeStillOpen(let path):
            "Persistent store is still open: \(path)"
        case .engineStartFailed(let component, let reason):
            "Engine could not start (\(component)): \(reason)"
        case .observationUnavailable(let reason):
            "Observation could not be established: \(reason)"
        case .concurrentNext:
            "A next()/signed() call was awaited while a previous one was still in flight; observation streams are single-consumer"
        case .factStreamLagged(let receiptId?):
            "The finite live fact stream fell behind; reattach receipt \(receiptId) to replay"
        case .factStreamLagged(receiptId: nil):
            "The finite live fact stream fell behind before a receipt was observable"
        case .receiptReplayUnavailable(let receiptId):
            "Retained evidence for receipt \(receiptId) became unavailable during replay"
        case .receiptClosedWithoutOutcome(let receiptId):
            "Receipt \(receiptId) closed before its terminal outcome"
        case .signEventAlreadyConsumed:
            "This sign-event result was already consumed"
        case .invalidSignature(let got):
            "Invalid signature hex: \(got.debugDescription)"
        case .engineClosed:
            "Engine already shut down"
        case .invalidNostrEntity(let reason):
            "Invalid Nostr entity: \(reason)"
        case .nostrEntitySecretKeyRejected:
            "Refusing to decode a secret-key entity"
        case .authorOutboxesRequiresBoundAuthors:
            "SourceAuthority.authorOutboxes requires a selection whose authors field is bound"
        case .emptyPinnedRelaySet:
            "SourceAuthority.pinned requires a nonempty relay set"
        case .windowZeroRows:
            "Window initial/max must be representable nonzero row counts"
        case .windowInitialExceedsMax(let initial, let max):
            "Window initial \(initial) exceeds max \(max)"
        case .windowSelectionHasLimit:
            "A windowed selection must not also declare a limit"
        case .windowAggregateResultLimit:
            "A windowed observation must not also declare an aggregate result limit"
        case .emptyQueryUnion:
            "A live query must declare at least one demand branch"
        case .aggregateResultLimitZero:
            "An aggregate result limit of zero can never contain a row"
        case .nestedAggregateResultLimit:
            "A nested live-query branch must not declare its own aggregate result limit"
        case .tooManyQueryBranches(let requested, let maximum):
            "A live query supports at most \(maximum) demand branches; \(requested) were declared"
        case .relayInformationUnavailable(let kind):
            "Relay information unavailable: \(kind)"
        case .invalidCorrelationToken(let got, let reason):
            "Invalid correlation token \(got.debugDescription): \(reason)"
        // nmp-native:if nip22
        case .invalidNip73(let reason):
            "Invalid NIP-73 external content id: \(reason)"
        // nmp-native:endif
        // nmp-native:if nip25
        case .invalidReaction(let reason):
            "Invalid reaction: \(reason)"
        // nmp-native:endif
        // nmp-native:if nip22
        case .replaceableEditHasNoWireForm:
            "A replaceable edit crosses this boundary only inside the semantic method that owns its precondition, never as a payload"
        // nmp-native:endif
        // nmp-native:if nip29
        case .emptyRelayScope:
            "RelayScope.on requires a nonempty relay set -- a group must be hosted somewhere"
        case .groupCallerSuppliedContext:
            "A group write must not carry its own h tag; the group's retained id is the sole source of that tag"
        case .groupCallerSuppliedContextConstraint:
            "A group read selection must not already constrain #h; the group's retained id is the sole source of that row"
        case .groupCallerSuppliedTimeline:
            "A group read selection must not already declare since/until/limit; the group door owns that bound"
        case .emptyGroupSet:
            "A group write must name at least one group; an event with no h row is not in a group at all"
        case .groupContextMissing(let expected):
            "Event carries no h tag; expected groups \(expected)"
        case .groupContextMismatched(let found, let expected):
            "Event's h tags name groups \(found), expected groups \(expected)"
        case .groupContextRepeated(let repeated):
            "Event names groups \(repeated) in more than one h row"
        case .groupRecordsNotContextScoped(let kinds):
            "Kinds \(kinds) are NIP-29's own relay-signed group records: they key themselves by d, never by h, so no such event could ever match a group content read; observe the group's records instead"
        case .groupNoRecordSelected:
            "A group records observation must name at least one of the three relay-signed records"
        case .groupUserBatchEmpty:
            "A NIP-29 user operation must name at least one user"
        case .groupUserBatchConflictingRoles(let pubkey):
            "NIP-29 user operation names \(pubkey) with conflicting roles"
        case .groupIdSelectionNamesNoKind:
            "A group-record selection must name at least one of NIP-29's three relay-signed group record kinds"
        case .groupIdSelectionNotAGroupRecordKind(let kind):
            "Kind \(kind) is not one of NIP-29's three relay-signed group records; a group host is not authoritative for it"
        // nmp-native:endif
        }
    }
}

/// Runs `body`, translating any thrown `FfiError` into the ergonomic
/// `NMPError` -- the one seam every call into `NMPFFI` passes through.
func nmpRethrowing<T>(_ body: () throws -> T) throws -> T {
    do {
        return try body()
    } catch let error as FfiError {
        throw NMPError(error)
    }
}

/// Async counterpart used by generated UniFFI operations. The suspension
/// remains visible to callers while preserving the ergonomic error surface.
func nmpRethrowingAsync<T>(_ body: () async throws -> T) async throws -> T {
    do {
        return try await body()
    } catch let error as FfiError {
        throw NMPError(error)
    }
}
