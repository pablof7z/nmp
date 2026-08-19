//! Engine construction and the two live accounts.
//!
//! ## Constructors
//!
//! The suspicion said nine. The count is exact and the shape is the point.
//! Six are ordinary public API:
//!
//! ```text
//! Engine::new(config)
//! Engine::new_with_capabilities(config, caps)
//! Engine::new_with_capabilities_and_routing(config, caps, route)
//! Engine::new_with_session(config, payload)
//! Engine::new_with_session_and_capabilities(config, payload, caps)
//! Engine::new_with_session_capabilities_and_routing(config, payload, caps, route)
//! ```
//!
//! Three more (`from_parts`, `from_parts_with_fixture_routing_facts`,
//! `from_parts_with_fixture_routing_facts_and_route_provider`) are
//! `#[doc(hidden)]` behind the `unstable-mechanism` feature. Nine total, six
//! reachable.
//!
//! The six vary over exactly three axes -- restored session, compiled
//! capabilities, author-route provider -- and they are NOT independent. There
//! is no `new_with_routing`. Reaching the route provider requires passing a
//! capability vec, so an app with an outbox and no capabilities writes
//! `Vec::new()` as a positional argument, and an app with capabilities and no
//! outbox writes `None`. Every one of the three is a value with a natural
//! "nothing" -- an empty vec, an `Option`, an absent payload -- so the six
//! names encode a combination the arguments already encode.
//!
//! Meanwhile `EngineConfig` has six fields (`store_path`, `app_relays`,
//! `fallback_relays`, `max_relays`, `max_auth_capabilities`,
//! `max_publish_attempts`) and none of them is any of the three axes. The
//! config carries what an app tunes; the constructor name carries what an app
//! composes. That split is defensible -- a `Box<dyn AuthorRouteProvider>` is
//! not a config field -- but it means construction is spelled in two places and
//! the app has to know which half a given knob lives in.
//!
//! ## The capability trap
//!
//! `new_with_capabilities` is documented as the door for "the complete compiled
//! replaceable-capability set available before store recovery", and "a retained
//! operation whose program/format is absent from `capabilities` refuses open
//! and leaves the store unchanged". So an app that calls
//! `nmp_nip02::set_following` once, then ships a version that forgets
//! `follow_capability()`, fails to START. The dependency runs from a WRITE made
//! months ago to a CONSTRUCTOR argument, and nothing at the write site says so.
//! [`Canary::CAPABILITIES`] below is this app's answer: name every capability
//! it could ever write, always, in one place.
//!
//! ## Two accounts
//!
//! `add_private_key_account(&[u8; 32], make_current)` twice, then
//! `make_current_account(key)` to switch. This works and is pleasant. The one
//! thing it does not give you is per-account state: `IdentityField::ActivePubkey`
//! is singular (see `notifications`), and `SessionSnapshot::current_pubkey` is
//! the only reactive identity, so "the other account's unread count" is a
//! literal-bound query the app maintains by hand.

use std::sync::Arc;

use nmp::{Engine, EngineConfig, PublicKey, ReplaceableMaterializerSpec, SessionAccount};

/// The whole app's engine plus its two accounts.
pub struct Canary {
    pub engine: Arc<Engine>,
    pub accounts: Vec<SessionAccount>,
}

impl Canary {
    /// Every replaceable capability this app can ever write, named once.
    ///
    /// Forgetting one is a cold-start failure on a device that already made
    /// that kind of write. There is no way to ask the store which programs it
    /// retains before opening it, so the only safe policy is "name them all".
    #[must_use]
    pub fn capabilities() -> Vec<ReplaceableMaterializerSpec> {
        vec![
            nmp_nip02::follow_capability(),
            nmp_nip29::group_list_capability(),
        ]
    }

    /// Open the engine.
    ///
    /// `store_path: None` is an in-memory store, which is what the exerciser
    /// wants. Note that `store_path` is an `Option<String>` and not a
    /// `PathBuf`: a filesystem path arrives as a `String` here while relay URLs
    /// arrive as `Vec<String>` and are parsed internally, so `EngineConfig` is
    /// the one place on the surface that takes stringly-typed values the rest
    /// of the API insists on having decoded.
    pub fn open(
        store_path: Option<String>,
        indexers: Vec<nmp::RelayUrl>,
    ) -> Result<Self, nmp::EngineError> {
        let config = EngineConfig {
            store_path,
            ..EngineConfig::default()
        };
        // The NIP-65 outbox provider: an app names the algorithm crate and
        // passes an instance. This is the surface working as designed -- and
        // it is also why `Vec::new()` can never be the argument in this
        // position: routing is only reachable through the three-argument form.
        let route_provider: Option<Box<dyn nmp::AuthorRouteProvider>> = if indexers.is_empty() {
            None
        } else {
            Some(Box::new(nmp_outbox::Nip65Outbox::new(indexers)))
        };
        let engine = Engine::new_with_capabilities_and_routing(
            config,
            Self::capabilities(),
            route_provider,
        )?;
        Ok(Self {
            engine: Arc::new(engine),
            accounts: Vec::new(),
        })
    }

    /// Add an account from raw secret bytes.
    ///
    /// `[u8; 32]` rather than an nsec or a hex string -- decoded at the app's
    /// own boundary, exactly as the convention says. Good.
    pub fn add_account(
        &mut self,
        secret: &[u8; 32],
        make_current: bool,
    ) -> Result<PublicKey, nmp::SessionMutationError> {
        let account = self.engine.add_private_key_account(secret, make_current)?;
        let key = account.public_key;
        self.accounts.push(account);
        Ok(key)
    }

    pub fn switch_to(&self, key: PublicKey) -> Result<(), nmp::SessionMutationError> {
        self.engine.make_current_account(key)
    }

    #[must_use]
    pub fn current(&self) -> Option<PublicKey> {
        self.engine.session().ok().and_then(|s| s.current_pubkey)
    }

    /// Engine-global diagnostics.
    ///
    /// The suspicion is confirmed: `Engine::observe_diagnostics()` is the only
    /// door, it is engine-global, and no query handle reaches it.
    /// `Subscription` has `recv`, `recv_timeout`, `request_rows`,
    /// `window_handle`, `cancel`, `cancel_handle` -- and nothing that says
    /// "how is THIS query doing". The per-query facts that DO exist arrive on
    /// `Frame` (`evidence`, `execution`), so a screen that has never received a
    /// frame has no diagnostics at all, and a screen whose observation is idle
    /// cannot ask.
    ///
    /// The two are also different vocabularies for the same subject.
    /// `RelayDiagnosticsSnapshot.coverage` is
    /// `Vec<FilterCoverageEntry { filter: String, coverage: Option<CoverageInterval> }>`
    /// where `filter` is "the EXACT wire JSON" -- a rendered string. `Frame.evidence`
    /// is `Vec<AcquisitionEvidence>` keyed by branch INDEX, whose
    /// `SourceEvidence` carries a `reconciled_through: Option<Timestamp>` and
    /// no filter at all. So "which relay is behind on the query on this
    /// screen" is answerable only by re-rendering the app's own `Demand` into
    /// the engine's exact wire JSON and string-matching it. The app does not
    /// have that renderer: `ConcreteFilter` is not re-exported.
    pub fn diagnostics(&self) -> Result<nmp::DiagnosticsSubscription, nmp::EngineError> {
        self.engine.observe_diagnostics()
    }

    pub fn shutdown(&self) {
        self.engine.shutdown();
    }
}
