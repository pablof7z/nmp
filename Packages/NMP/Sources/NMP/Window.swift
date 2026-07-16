// Windowing is a POLICY on the read noun (#485), never a parallel noun:
// there is no separate bounded-query type, only `NMPEngine.observe`
// with an optional `Window`. Delivery mode is DERIVED from boundedness --
// unbounded observations stream exact rebased deltas (intermediate reducer
// emits may conflate; full-set redelivery is the known O(rows²) class),
// bounded observations deliver conflated
// authoritative snapshots. Growth is declarative (`requestRows(atLeast:)`),
// never token-shaped.

import NMPFFI

/// Window policy for `NMPEngine.observe`. Passing `nil` keeps today's
/// unbounded delta observation; passing a window bounds the observation to
/// a newest-first row set delivered as authoritative snapshots.
///
/// One real variant today; future policies (latest/anchored) arrive as new
/// variants of this same enum, not as new nouns or new observe verbs.
public enum Window: Sendable, Equatable {
    /// Bounded newest-first window: opens with `initial` canonical rows
    /// (`createdAt DESC, id ASC`) and grows only by an explicit
    /// `NMPQuery.requestRows(atLeast:)`, never above `max`. Both counts
    /// must be nonzero and `initial <= max` -- `observe` throws
    /// `NMPError.windowZeroRows` / `.windowInitialExceedsMax` otherwise.
    case expandable(initial: UInt64, max: UInt64)

    func toFfi() -> FfiWindow {
        switch self {
        case .expandable(let initial, let max):
            return .expandable(initial: initial, max: max)
        }
    }
}

/// Mechanical growth state of an expandable window, delivered as a fact on
/// every windowed `RowBatch` (`RowBatch.load`). Deliberately separate from
/// acquisition evidence, and deliberately WITHOUT a Complete/End/Synced
/// variant: `.returned(added: 0)` only means the planned advance added no
/// canonical row -- never that no older event exists anywhere (read the
/// per-source evidence for that judgment).
///
/// `.atBound(max:)` is likewise a FACT, not a failure: a `requestRows` call
/// that cannot raise the target past the declared `max` still lands here,
/// as a delivered beat, so the caller always observes the outcome of its
/// request in-band.
public enum WindowLoad: Sendable, Equatable {
    case idle
    case requesting
    case returned(added: UInt64)
    case atBound(max: UInt64)

    init(_ ffi: FfiWindowLoad) {
        switch ffi {
        case .idle: self = .idle
        case .requesting: self = .requesting
        case .returned(let added): self = .returned(added: added)
        case .atBound(let max): self = .atBound(max: max)
        }
    }
}

/// Typed failures from `NMPQuery.requestRows(atLeast:)`. Only genuinely
/// synchronous refusals live here -- growth OUTCOMES (including reaching
/// the window's bound) arrive as `WindowLoad` facts in delivered batches,
/// never as thrown errors. There is deliberately no stale-token or
/// load-in-progress case: `requestRows` is monotonic and idempotent, so
/// there is no token to misuse and no in-flight state to collide with.
public enum NMPRequestRowsError: Error, Sendable, Equatable {
    /// This query observes the full live set; there is no window to grow.
    case unwindowed
    case engineClosed
    /// The canonical store could not serve the advance (the staged load
    /// was rolled back; delivered state is untouched).
    case storeUnavailable
    /// No planned source could serve the advance (the staged load was
    /// rolled back; delivered state is untouched).
    case transportUnavailable(reason: String)

    init(_ ffi: FfiRequestRowsError) {
        switch ffi {
        case .Unwindowed: self = .unwindowed
        case .EngineClosed: self = .engineClosed
        case .StoreUnavailable: self = .storeUnavailable
        case .TransportUnavailable(let reason):
            self = .transportUnavailable(reason: reason)
        }
    }
}
