// App-owned query ergonomics (M5 plan §3: "Keeping it app-side is
// deliberate -- proves the app, not NMP, owns its query ergonomics"). `NMP`
// itself exposes nothing named "follows" or "relay list" -- only the
// general `NMPFilter`/`NMPBinding` algebra. This file is the falsifier's own
// two named query shapes, built from that algebra.

import NMP

enum FeedFilters {
    /// "$myFollows at depth 1": kind:X events authored by whoever the
    /// active account's kind:3 contact list currently names (their `p`
    /// tags). Reactive -- re-resolves live whenever the active account's
    /// kind:3 changes, with NO re-`observe` call from this app.
    static func follows(kinds: [UInt16]) -> NMPFilter {
        NMPFilter(
            kinds: kinds,
            authors: .derived(
                inner: NMPDemand(
                    selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                    source: .authorOutboxes
                ),
                project: .tag("p")
            ),
            limit: 200
        )
    }

    /// The active account's follows' own NIP-65 relay lists (kind:10002),
    /// same reactive derivation as `follows(kinds:)` but projecting a
    /// different kind. The app aggregates the `r` tags client-side to rank
    /// relays -- there is no `relays:` filter parameter to hand any of this
    /// back into routing (RelaysView renders that absence).
    static func followsRelayLists() -> NMPFilter {
        NMPFilter(
            kinds: [10_002],
            authors: .derived(
                inner: NMPDemand(
                    selection: NMPFilter(kinds: [3], authors: .reactive(.activePubkey)),
                    source: .authorOutboxes
                ),
                project: .tag("p")
            )
        )
    }
}
