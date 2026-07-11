//! External-consumer closure proof for the `nmp` facade (#52 acceptance:
//! "an app's `Cargo.toml` names `nmp` alone"). This crate's own
//! `Cargo.toml` depends on `nmp` ONLY -- no mechanism crate, and not even
//! `nostr` directly: every value type below is reached through `nmp`'s own
//! re-exports. If this crate fails to compile, the facade's re-export
//! inventory has a gap.
//!
//! Every fixture here uses ARBITRARY caller-owned kinds (9998/9999), never
//! kind:1/kind:3 or any other NIP-01 core schema. `docs/known-gaps.md`'s v2
//! contract promotion is explicit: "No kind:1-first core catalog is part of
//! the target" -- a facade acceptance proof that hardcodes the
//! follows/feed shape would bake exactly the kind bias that promotion
//! forbids into the canonical surface's own story. Everything below proves
//! the GRAMMAR/write-plane/diagnostics MECHANICS are reachable from `nmp`
//! alone; it asserts nothing about what any particular kind means.
//!
//! Exercises, from `nmp` alone:
//! - the grammar a `LiveQuery` is built from ([`build_derived_index_query`]);
//! - the advertised unsigned-write path ([`build_unsigned_intent`]) --
//!   `UnsignedEvent`/`Kind`/`Tag`/`Timestamp` were the exact re-exports a
//!   prior review found missing;
//! - naming every `DiagnosticsSnapshot` output type, not just some of them
//!   ([`describe_snapshot`]/[`describe_relay`]/[`describe_coverage_entry`]) --
//!   `DiagnosticsSnapshot`, `RelayDiagnosticsSnapshot`, `FilterCoverageEntry`,
//!   `QueryCoverage`, and `Lane` are each named as an explicit type, not just
//!   imported and left unused past one field read.
//!
//! The `#[cfg(test)]` module below additionally drives a real `Engine`
//! end-to-end (construct, `add_account`, `observe`, `publish`,
//! `observe_diagnostics`, `shutdown`) with no relays configured -- proving
//! the two nouns are not just nameable but usable.

use nmp::{
    Derived, DiagnosticsSnapshot, Durability, Filter, FilterCoverageEntry, IdentityField, Kind,
    Lane, LiveQuery, PublicKey, QueryCoverage, RelayDiagnosticsSnapshot, Selector, Tag, TagName,
    Timestamp, UnsignedEvent, WriteIntent, WritePayload, WriteRouting,
};

/// The reactive index kind an app might declare its own membership list
/// under -- arbitrary, caller-owned, and meaningless to `nmp` itself.
const CALLER_INDEX_KIND: u16 = 9998;
/// The content kind that index's projected tag identifies authors of --
/// likewise arbitrary and caller-owned.
const CALLER_CONTENT_KIND: u16 = 9999;

/// A caller-owned derived-index query shape: kind `9999` content authored
/// by whoever the active pubkey's kind `9998` "index" event currently names
/// via its `p` tags. Structurally identical to the reactive-derived-set
/// shape this repo's other falsifiers build (a `Derived`/`Reactive`/
/// `Tag`-projection), just re-kinded onto two arbitrary caller-owned kinds
/// instead of any NIP-01 core schema -- proves `Filter`/`Binding`/`Derived`/
/// `Selector`/`IdentityField`/`TagName` are all nameable and constructible
/// from `nmp` alone, without asserting anything about what a specific kind
/// means.
pub fn build_derived_index_query() -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(std::collections::BTreeSet::from([CALLER_CONTENT_KIND])),
        authors: Some(nmp::Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(std::collections::BTreeSet::from([CALLER_INDEX_KIND])),
                authors: Some(nmp::Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag(TagName::new('p').expect("'p' is a valid tag name")),
        }))),
        ..Filter::default()
    })
}

/// Proves an unsigned `WriteIntent` is fully constructible from `nmp` alone
/// -- the advertised unsigned-write path (`UnsignedEvent`/`Kind`/`Tag`/
/// `Timestamp`). Uses the same arbitrary caller-owned content kind as
/// [`build_derived_index_query`], never a NIP-01 core kind.
pub fn build_unsigned_intent(author: PublicKey, content: &str) -> WriteIntent {
    let unsigned = UnsignedEvent::new(
        author,
        Timestamp::now(),
        Kind::Custom(CALLER_CONTENT_KIND),
        Vec::<Tag>::new(),
        content,
    );
    WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Ephemeral,
        routing: WriteRouting::AuthorOutbox,
    }
}

/// Names `FilterCoverageEntry` AND `QueryCoverage` as explicit types (not
/// merely a field read through `Debug`) -- proves both resolve from `nmp`
/// alone.
pub fn describe_coverage_entry(entry: &FilterCoverageEntry) -> String {
    let coverage: &QueryCoverage = &entry.coverage;
    format!("{}: {coverage:?}", entry.filter)
}

/// Names `RelayDiagnosticsSnapshot` and `Lane` as explicit types, and calls
/// through to [`describe_coverage_entry`] for every one of its coverage
/// entries -- so removing ANY of `RelayDiagnosticsSnapshot`/`Lane`/
/// `FilterCoverageEntry`/`QueryCoverage` from `nmp`'s re-exports breaks this
/// crate, not just a claim in a doc comment.
pub fn describe_relay(snapshot: &RelayDiagnosticsSnapshot) -> String {
    let lanes: Vec<Lane> = snapshot.by_lane.iter().map(|(lane, _)| *lane).collect();
    let coverage: Vec<String> = snapshot
        .coverage
        .iter()
        .map(describe_coverage_entry)
        .collect();
    format!(
        "{} subs on {} across lanes {lanes:?}; coverage: [{}]",
        snapshot.wire_sub_count,
        snapshot.relay,
        coverage.join(", "),
    )
}

/// Names the TOP-LEVEL `DiagnosticsSnapshot` type itself -- a prior version
/// of this proof imported `RelayDiagnosticsSnapshot`/`Lane` but never named
/// `DiagnosticsSnapshot` or `FilterCoverageEntry` anywhere, so the facade
/// could have dropped either re-export without this crate noticing. This
/// function's parameter type closes that gap.
pub fn describe_snapshot(snapshot: &DiagnosticsSnapshot) -> String {
    let relays: Vec<String> = snapshot.relays.iter().map(describe_relay).collect();
    format!(
        "{} uncovered authors; relays: [{}]",
        snapshot.uncovered_author_count,
        relays.join("; "),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use nmp::{Engine, EngineConfig};

    /// A fixed, valid secp256k1 secret key -- generated once via `openssl
    /// rand -hex 32`. Hardcoded rather than derived from `nostr::Keys`
    /// because this crate has no dependency on `nostr` at all (the whole
    /// point of this crate).
    const TEST_SECRET_KEY_HEX: &str =
        "32f6df73ead850b6e13c0649846b7a1d9646d6a0b50c69361981176e817e70f8";

    /// Drives `Engine::new`/`add_account`/`observe`/`publish`/
    /// `observe_diagnostics`/`shutdown` end-to-end from this `nmp`-only
    /// crate, with no relays configured (no network needed) -- the two
    /// nouns are not merely nameable, they are usable.
    #[test]
    fn engine_two_nouns_are_usable_from_nmp_alone() {
        let engine = Engine::new(EngineConfig::default()).expect("in-memory engine must build");

        let author = engine
            .add_account(TEST_SECRET_KEY_HEX)
            .expect("fixed test secret key must parse");

        let subscription = engine
            .observe(build_derived_index_query())
            .expect("engine is open");
        drop(subscription); // explicit early withdraw, exercised via Drop

        let receipts = engine
            .publish(build_unsigned_intent(
                author,
                "hello from an nmp-only consumer",
            ))
            .expect("engine is open");
        drop(receipts);

        let diagnostics = engine.observe_diagnostics().expect("engine is open");
        if let Some(snapshot) = diagnostics.recv() {
            let _ = describe_snapshot(&snapshot);
        }

        engine.shutdown();
    }
}
