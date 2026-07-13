//! The write-intent vocabulary (#115 Fable ruling, Fork 3's dependency
//! ruling): `Durability`, `WritePayload`, `WriteIntent`, `WriteRouting`,
//! `NarrowOnly`, `PrivateRoute`, and `HostAuthority` relocate here from
//! `nmp-engine::outbox` -- a protocol module (e.g. `nmp-nip29`) composing a
//! `WriteIntent` must not gain an engine dependency to do so, and this
//! crate is already the read noun's home (`Demand`/`SourceAuthority`), so
//! it is the write noun's correct home too: the write vocab living in the
//! reducer crate was a layering accident #115 is first to trip over, not a
//! deliberate boundary. `WriteStatus`/`Receipt`/`ReceiptSink` stay in
//! `nmp-engine` (they reference `core::ReceiptId` and are runtime evidence,
//! not intent vocab -- an app never constructs one).
//!
//! Hard break, no compatibility alias: every caller in the workspace moved
//! to `nmp_grammar::{Durability, WriteIntent, ...}` in the same change.

use std::collections::BTreeSet;

use nostr::{Event as SignedEvent, EventId, PublicKey, RelayUrl, UnsignedEvent};

/// A typed property of a write (M0 amendment) — not a routing choice.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum Durability {
    Durable,
    Ephemeral,
    AtMostOnce,
}

/// The event payload of a write intent. VISION P states signing and
/// publishing are ORTHOGONAL stages, not one linear lifecycle: a caller
/// that already holds a validly-signed event (e.g. republishing a
/// previously-signed private event to a recomputed narrow relay set,
/// ledger #6) supplies `Signed` and skips `Effect::RequestSign` entirely,
/// going straight to routing; a caller with a template supplies `Unsigned`
/// and the reducer requests the signer capability.
pub enum WritePayload {
    Unsigned(UnsignedEvent),
    /// An unsigned whole-value replacement whose acceptance is conditional
    /// on the store still holding exactly `expected_base` at the draft's
    /// replaceable/addressable coordinate. `None` means "there is still no
    /// local winner"; it never means that Nostr is globally empty.
    ///
    /// The precondition travels with the draft so a protocol module can
    /// compose one closed, race-free write value. It is checked inside the
    /// store's atomic acceptance transaction, before an intent/receipt id is
    /// allocated or any canonical row is changed. This variant is unsigned
    /// for the same reason as [`Self::Unsigned`]: NMP freezes and signs the
    /// exact body after acceptance.
    UnsignedReplaceableEdit {
        unsigned: UnsignedEvent,
        expected_base: Option<EventId>,
    },
    Signed(SignedEvent),
}

/// A caller's publish request.
pub struct WriteIntent {
    pub payload: WritePayload,
    pub durability: Durability,
    pub routing: WriteRouting,
}

/// Where a `WriteIntent` is routed.
#[derive(Clone)]
pub enum WriteRouting {
    /// The author's write relays (reuses the M2 router's lanes).
    AuthorOutbox,
    /// Recipients' inboxes (kind:10050 / NIP-65 read).
    ToInboxes(Vec<PublicKey>),
    /// Ledger #6: narrow-only, fail-closed.
    PrivateNarrow(PrivateRoute),
    /// #115: an explicit, single pinned host authority -- the write-side
    /// analog of [`crate::SourceAuthority::Pinned`]. NOT `PrivateNarrow`:
    /// that variant is ledger #6's private/fail-closed narrow route, and
    /// reusing it here would make host-authority evidence lie (a
    /// `PrivateNarrow` receipt means "the caller pre-narrowed this exact
    /// set," not "this is the group's host"). Kind-blind, protocol-blind:
    /// the engine's `resolve_routes` treats this as "route to exactly this
    /// one relay," nothing more -- it never knows or cares this is a
    /// NIP-29 group host. See [`HostAuthority`]'s own doc for the
    /// misuse-resistance story (the FFI tier withholds this variant
    /// entirely -- an app can only reach it transitively through a
    /// protocol module's composed intent).
    PinnedHost(HostAuthority),
}

/// Fail-closed narrow relay set (ledger #6). By construction this type
/// exposes no widen/insert-arbitrary operation: `new` is the ONLY way to
/// populate it (a one-shot, fixed set at construction time — the caller
/// must already have resolved and narrowed this itself), and no
/// insert/extend/union method exists afterward. A `PrivateNarrow` intent
/// whose set is empty is exactly how an unroutable private recipient is
/// expressed structurally — the reducer fails it CLOSED (`WriteStatus::
/// Failed`), it never falls back to a public write relay, because there is
/// no operation that could hand it one.
#[derive(Debug, Clone, Default)]
pub struct NarrowOnly<T> {
    items: BTreeSet<T>,
}

impl<T: Ord> NarrowOnly<T> {
    /// Construct a narrow, FIXED relay set. No widen operation exists on
    /// this type — an empty set is legal and is how "unroutable" is
    /// expressed.
    pub fn new(items: impl IntoIterator<Item = T>) -> Self {
        Self {
            items: items.into_iter().collect(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    pub fn iter(&self) -> std::collections::btree_set::Iter<'_, T> {
        self.items.iter()
    }
}

#[derive(Clone)]
pub struct PrivateRoute {
    pub relays: NarrowOnly<RelayUrl>,
}

/// An explicit, single pinned write-host authority (#115) — the write-side
/// analog of [`crate::SourceAuthority::Pinned`] (#107): read-side parity is
/// the standard this type follows exactly. `host` is PRIVATE: `new()` (via
/// [`Self::from_selected_host`]) is the only mint, mirroring
/// `SourceAuthority::Pinned`'s own newtype discipline. Singleton, not a
/// set — a NIP-29 group lives on exactly one host, so there is nothing to
/// widen/union the way a read-side pinned scope might legitimately name
/// several relays at once.
///
/// **Misuse-resistance story (Fable ruling, Fork 1):** `from_selected_host`
/// is `pub` and infallible at the direct-Rust tier, exactly as
/// `SourceAuthority::Pinned`'s constructor is — a trusted protocol module
/// (like `nmp-nip29`) or a direct-Rust caller who has ALREADY established
/// which relay is the correct host may mint one. This is NOT a
/// cryptographically-sealed capability token (that would be theater at a
/// Rust API boundary: Rust cannot express "only `nmp-nip29` may call this"
/// without inverting the dependency graph). The REAL enforcement is at the
/// FFI tier: `nmp-ffi` withholds both a `HostAuthority` constructor AND any
/// matching `FfiWriteRouting` variant entirely (see that crate's
/// `convert.rs`/`nip29.rs`) — an app can only ever obtain a pinned write
/// transitively, through a protocol module's already-composed
/// `WriteIntent` (`nmp_nip29::compose_group_send`), never by naming a host
/// itself. Direct-Rust callers are the trusted tier where this constructor
/// being public is exactly the same posture as every other `Pinned`
/// authority in this crate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct HostAuthority {
    host: RelayUrl,
}

impl HostAuthority {
    /// Mint a `HostAuthority` for `host` — the caller (a protocol module,
    /// or a direct-Rust app that already knows its selected host) asserts
    /// this IS the correct host. Infallible: unlike
    /// `SourceAuthority::Pinned`'s relay SET (which can be empty),
    /// `RelayUrl` itself already guarantees a well-formed single URL by
    /// construction — there is no analogous "empty" state to reject.
    pub fn from_selected_host(host: RelayUrl) -> Self {
        Self { host }
    }

    pub fn host(&self) -> RelayUrl {
        self.host.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_authority_round_trips_the_selected_host() {
        let host = RelayUrl::parse("wss://host.example.com").unwrap();
        let auth = HostAuthority::from_selected_host(host.clone());
        assert_eq!(auth.host(), host);
    }
}
