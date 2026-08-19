//! The person sheet: follow, unfollow, mute, and the button's own state.
//!
//! ## What we wanted to write
//!
//! ```text
//! if people.follows(target) { "Unfollow" } else { "Follow" }   // a bool, now
//! people.follow(target)?;                                      // a semantic edit
//! ```
//!
//! ## What we found
//!
//! ### Follow / unfollow is the best door in the whole surface
//!
//! `nmp_nip02::set_following(&engine, &writes, target, FollowChange::Follow)`
//! is a genuine semantic edit: the app never reads the kind:3, never
//! reconstructs the tag set, never races another device. It compiles to a
//! `ReplaceableMaterializerSpec` the engine replays after a restart, freezes
//! the author at acceptance, and supplies an empty kind:3 when none exists.
//! This is what every other list operation in this app should look like.
//!
//! The cost is one line in `main`: the capability must be handed to
//! `Engine::new_with_capabilities` BEFORE store recovery, or a retained
//! follow operation refuses the store open. An app that forgets is not told
//! at the call site; it is told at the next cold start, by a construction
//! error. See `app::Canary::open`.
//!
//! ### Mute is not a door at all
//!
//! There is no mute capability. `feed::MUTE_LIST_KIND` is declared in this app.
//! Muting therefore means: read the current kind:10000, append a `p` row,
//! publish the whole replaced list -- the exact read-modify-write race
//! `follow_writes` exists to abolish, performed by hand, one file away from the
//! thing that abolishes it. [`Mutes::mute`] below is that code.
//!
//! ### The predicate is a stream, and the stream cannot answer synchronously
//!
//! A button needs a `bool` at paint time. What exists is
//! `nmp_nip02::observe_following(engine, target) -> FollowObservation`, whose
//! `recv()` blocks for the NEXT snapshot -- so the first paint has nothing.
//! `FollowSnapshot` carries the right answer (`FollowRelationship::{Unknown,
//! NotFollowing, Following}` plus a separate `FollowAvailability`, which is
//! the correct split), but there is no `latest()` accessor to read what has
//! already been folded. `nmp-nip29`'s `GroupObservation` HAS `latest()`;
//! `FollowObservation` does not. Two observation types in two capability
//! crates, one of them answerable at paint time.
//!
//! [`FollowButton`] below is the app-side latest-slot that has to exist because
//! of it -- a third copy of the same latest-wins fold, after `RowTable` and
//! `ProfileBook`.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use nmp::{
    Engine, EventBuilder, Identity, Kind, PublicKey, ReceiptStream, Row, WriteIntent, WritePayload,
    WriteRouting,
};
use nmp_nip02::{FollowChange, FollowRelationship, FollowSnapshot, FollowWrites};

use crate::feed::MUTE_LIST_KIND;

/// Follow / unfollow, through the capability that owns them.
pub struct Follows {
    writes: FollowWrites,
}

impl Default for Follows {
    fn default() -> Self {
        Self::new()
    }
}

impl Follows {
    #[must_use]
    pub fn new() -> Self {
        Self {
            writes: nmp_nip02::follow_writes(),
        }
    }

    /// The capability an `Engine` must be constructed with for the two verbs
    /// below to be publishable at all.
    #[must_use]
    pub fn capability() -> nmp::ReplaceableMaterializerSpec {
        nmp_nip02::follow_capability()
    }

    pub fn follow(
        &self,
        engine: &Engine,
        target: PublicKey,
    ) -> Result<ReceiptStream, nmp_nip02::FollowActionFailure> {
        nmp_nip02::set_following(engine, &self.writes, target, FollowChange::Follow)
    }

    pub fn unfollow(
        &self,
        engine: &Engine,
        target: PublicKey,
    ) -> Result<ReceiptStream, nmp_nip02::FollowActionFailure> {
        nmp_nip02::set_following(engine, &self.writes, target, FollowChange::Unfollow)
    }

    /// "Does this row's kind:3 name `target`?" -- `nmp_nip02::follows(&Row,
    /// PublicKey)`. A pure predicate over a row the app already holds, and the
    /// only NIP-02 membership reader that does not need an observation.
    #[must_use]
    pub fn row_names(contact_list: &Row, target: PublicKey) -> bool {
        nmp_nip02::follows(contact_list, target)
    }
}

/// The follow button's state, as a value a paint pass can read.
///
/// Exists because `FollowObservation` has `recv()`/`recv_timeout()` and no
/// `latest()`. A background pump owns the observation and writes here; the UI
/// thread reads. This is the third latest-wins slot in this app, and the
/// second one written to work around an observation type that folds internally
/// and then refuses to show the fold.
pub struct FollowButton {
    latest: Arc<Mutex<Option<FollowSnapshot>>>,
    cancel: nmp::ObservationCancel,
    pump: Option<std::thread::JoinHandle<()>>,
}

impl FollowButton {
    /// Open the observation and start pumping it into a readable slot.
    ///
    /// Note the `Arc<Engine>`: `observe_following` takes ownership of one,
    /// while every other verb on this type takes `&Engine`. An app therefore
    /// holds its engine in an `Arc` because ONE capability function in ONE
    /// crate wants it that way.
    ///
    /// Note also the thread. `FollowObservation::recv` blocks, so reading the
    /// relationship without stalling a paint pass needs a thread per open
    /// person sheet. The non-blocking twin, `observe_following_async`, returns
    /// an `AsyncFollowObservation` whose `next()` is a future -- and neither
    /// `nmp` nor `nmp-nip02` exports an executor to await it on, so using it
    /// means the app depends on `tokio` directly and guesses at a runtime
    /// configuration compatible with the engine's own.
    ///
    /// `nmp::nmp_threads_spawned()` counts NMP's threads. It does not count
    /// this one, and this one exists only because of NMP's delivery shape.
    pub fn open(engine: Arc<Engine>, target: PublicKey) -> Result<Self, nmp::EngineError> {
        let observation = nmp_nip02::observe_following(engine, target)?;
        let cancel = observation.cancel_handle();
        let latest: Arc<Mutex<Option<FollowSnapshot>>> = Arc::new(Mutex::new(None));
        let sink = latest.clone();
        let pump = std::thread::spawn(move || {
            let observation = observation;
            while let Some(snapshot) = observation.recv() {
                *sink.lock().unwrap() = Some(snapshot);
            }
        });
        Ok(Self {
            latest,
            cancel,
            pump: Some(pump),
        })
    }

    /// What the button says right now. `Unknown` before the first delivery --
    /// which is the honest answer and also the one a button cannot render, so
    /// every app picks a default and this is where the bug goes.
    #[must_use]
    pub fn relationship(&self) -> FollowRelationship {
        self.latest
            .lock()
            .unwrap()
            .as_ref()
            .map_or(FollowRelationship::Unknown, |snapshot| {
                snapshot.relationship
            })
    }

    #[must_use]
    pub fn snapshot(&self) -> Option<FollowSnapshot> {
        self.latest.lock().unwrap().clone()
    }

    /// Wait until the first snapshot lands, or give up. Exists purely so the
    /// exerciser can prove the slot fills; a real UI paints `Unknown` and
    /// repaints later.
    pub fn wait(&self, timeout: std::time::Duration) -> Option<FollowSnapshot> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(snapshot) = self.snapshot() {
                return Some(snapshot);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
    }
}

impl Drop for FollowButton {
    fn drop(&mut self) {
        self.cancel.cancel();
        if let Some(handle) = self.pump.take() {
            let _ = handle.join();
        }
    }
}

/// Muting, by hand, because nothing owns kind:10000.
///
/// Every line below is a line `nmp-nip02` deleted for follows and nobody
/// deleted for mutes: the app reads its own current list, computes the new tag
/// set, and publishes a whole replacement. Two devices muting two different
/// people at the same moment lose one of the two edits, and this app has no
/// way to prevent that.
pub struct Mutes;

impl Mutes {
    /// The read declaration for my own mute list.
    #[must_use]
    pub fn my_mute_list() -> nmp::LiveQuery {
        nmp::LiveQuery::single(nmp::Demand {
            selection: nmp::Filter {
                kinds: Some(BTreeSet::from([MUTE_LIST_KIND])),
                authors: Some(nmp::Binding::Reactive(nmp::IdentityField::ActivePubkey)),
                ..nmp::Filter::default()
            },
            ..nmp::Demand::default()
        })
    }

    /// Is `target` in this mute-list row?
    ///
    /// The same function `nmp_nip02::follows` already is, re-written for a kind
    /// nobody owns. Note that it cannot even be delegated: `follows` is
    /// hard-wired to `p` on a kind:3 by name, and takes a `Row` whose kind it
    /// never checks -- so calling `nmp_nip02::follows(mute_list_row, target)`
    /// would actually WORK and would be a lie about which NIP the app is
    /// speaking. That accidental compatibility is worse than an error.
    #[must_use]
    pub fn muted(mute_list: &Row, target: PublicKey) -> bool {
        let target_hex = target.to_hex();
        mute_list.tags().iter().any(|tag| {
            let cells = tag.as_slice();
            cells.first().is_some_and(|cell| cell == "p")
                && cells.get(1).is_some_and(|cell| cell == &target_hex)
        })
    }

    /// Publish a mute list with `target` added.
    ///
    /// `current` is whatever the app's own observation last delivered, which
    /// may be nothing (first mute ever) or stale (another device just wrote
    /// one). Read-modify-write, no CAS, no materializer.
    pub fn mute(
        engine: &Engine,
        author: PublicKey,
        current: Option<&Row>,
        target: PublicKey,
    ) -> Result<ReceiptStream, nmp::EngineError> {
        Self::publish_list(engine, author, Self::with(current, target, true))
    }

    /// Publish a mute list with `target` removed.
    pub fn unmute(
        engine: &Engine,
        author: PublicKey,
        current: Option<&Row>,
        target: PublicKey,
    ) -> Result<ReceiptStream, nmp::EngineError> {
        Self::publish_list(engine, author, Self::with(current, target, false))
    }

    fn with(current: Option<&Row>, target: PublicKey, present: bool) -> Vec<String> {
        let target_hex = target.to_hex();
        let mut keys: Vec<String> = current
            .map(|row| crate::rows::tag_values(row, "p"))
            .unwrap_or_default();
        keys.retain(|key| key != &target_hex);
        if present {
            keys.push(target_hex);
        }
        keys.sort();
        keys.dedup();
        keys
    }

    fn publish_list(
        engine: &Engine,
        author: PublicKey,
        keys: Vec<String>,
    ) -> Result<ReceiptStream, nmp::EngineError> {
        let mut builder = EventBuilder::new(Kind::from(MUTE_LIST_KIND));
        for key in keys {
            // `nmp::Tag` is re-exported, and building a `p` row from a hex
            // string means naming `Tag::parse`, which is fallible on a value
            // the app just produced from a `PublicKey`. There is no
            // `Tag::public_key` on the re-exported surface... there is, on
            // `nostr::Tag`, which IS what is re-exported. So this works -- and
            // it works by reaching a `nostr` inherent method through a facade
            // re-export whose doc says the facade is the public API.
            if let Ok(key) = PublicKey::from_hex(&key) {
                builder = builder.tag(nmp::Tag::public_key(key));
            }
        }
        engine.publish(WriteIntent {
            payload: WritePayload::Event(builder),
            routing: WriteRouting::Auto,
            identity: Identity::Explicit(author),
        })
    }
}
