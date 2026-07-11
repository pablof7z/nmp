//! External-consumer closure proof for the `nmp` facade (#52 acceptance:
//! "an app's `Cargo.toml` names `nmp` alone"). This crate's own
//! `Cargo.toml` depends on `nmp` ONLY -- no mechanism crate, and not even
//! `nostr` directly: every value type below is reached through `nmp`'s own
//! re-exports. If this crate fails to compile, the facade's re-export
//! inventory has a gap.
//!
//! Exercises, from `nmp` alone:
//! - the grammar a `LiveQuery` is built from ([`build_follow_feed_query`]);
//! - the advertised unsigned-write path ([`build_unsigned_intent`]) --
//!   `UnsignedEvent`/`Kind`/`Tag`/`Timestamp` were the exact re-exports
//!   codex-nova's review found missing;
//! - naming every `DiagnosticsSnapshot` output field type
//!   ([`describe_relay`]) -- `RelayDiagnosticsSnapshot`/`FilterCoverageEntry`/
//!   `Lane` likewise.
//!
//! The `#[cfg(test)]` module below additionally drives a real `Engine`
//! end-to-end (construct, `add_account`, `observe`, `publish`, `observe_diagnostics`,
//! `shutdown`) with no relays configured -- proving the two nouns are not
//! just nameable but usable.

use nmp::{
    Derived, Durability, Filter, IdentityField, Kind, Lane, LiveQuery, PublicKey,
    RelayDiagnosticsSnapshot, Selector, Tag, TagName, Timestamp, UnsignedEvent, WriteIntent,
    WritePayload, WriteRouting,
};

/// The `$myFollows`-shaped query this repo's own falsifiers (`nmp-demo`,
/// `nmp-bdd`) build -- proves `Filter`/`Binding`/`Derived`/`Selector`/
/// `IdentityField`/`TagName` are all nameable and constructible from `nmp`
/// alone.
pub fn build_follow_feed_query() -> LiveQuery {
    LiveQuery(Filter {
        kinds: Some(std::collections::BTreeSet::from([1u16])),
        authors: Some(nmp::Binding::Derived(Box::new(Derived {
            inner: Filter {
                kinds: Some(std::collections::BTreeSet::from([3u16])),
                authors: Some(nmp::Binding::Reactive(IdentityField::ActivePubkey)),
                ..Filter::default()
            },
            project: Selector::Tag(TagName::new('p').expect("'p' is a valid tag name")),
        }))),
        ..Filter::default()
    })
}

/// Proves an unsigned `WriteIntent` is fully constructible from `nmp` alone
/// -- the advertised unsigned-write path that was NOT closed off before
/// this fix (`UnsignedEvent`/`Kind`/`Tag`/`Timestamp` were missing
/// re-exports).
pub fn build_unsigned_intent(author: PublicKey, content: &str) -> WriteIntent {
    let unsigned = UnsignedEvent::new(
        author,
        Timestamp::now(),
        Kind::TextNote,
        Vec::<Tag>::new(),
        content,
    );
    WriteIntent {
        payload: WritePayload::Unsigned(unsigned),
        durability: Durability::Ephemeral,
        routing: WriteRouting::AuthorOutbox,
    }
}

/// Names every diagnostics output type -- proves the diagnostics product
/// surface is closed (`DiagnosticsSnapshot`'s fields all resolve without a
/// mechanism-crate import: `RelayDiagnosticsSnapshot`, its `by_lane: Vec<(Lane,
/// usize)>`, and its `coverage: Vec<FilterCoverageEntry>`).
pub fn describe_relay(snapshot: &RelayDiagnosticsSnapshot) -> String {
    let lanes: Vec<Lane> = snapshot.by_lane.iter().map(|(lane, _)| *lane).collect();
    format!(
        "{} subs on {} across lanes {lanes:?}, {} filters proven",
        snapshot.wire_sub_count,
        snapshot.relay,
        snapshot.coverage.len(),
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
            .observe(build_follow_feed_query())
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
            for relay in &snapshot.relays {
                let _ = describe_relay(relay);
            }
        }

        engine.shutdown();
    }
}
