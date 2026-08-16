//! The engine thread's one owner for identity-session membership and
//! signing-capability availability (#1731). Moved out of `lib.rs` beside its
//! sibling owners (`nip11_decision`, `store_recovery`, `wire_admission`) —
//! same treatment already proven twice in the reducer (#1693, #1695): an
//! owner module with private fields, not a crate. Nothing here has an
//! independent dependency, consumer, or lifecycle; `engine_loop` still
//! decides ordering, this module still decides state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use nmp_engine::core::{self, ReceiptId};
use nmp_signer::{
    SignerOp, SignerPublicKey, SignerSignedEvent, SignerSignedEventParts, SignerUnsignedEvent,
    SigningCapability,
};
use nostr::{Event as SignedEvent, EventId, PublicKey, Tag, Timestamp, UnsignedEvent};

use crate::session::{SessionAccount, SessionProvider, SessionSnapshot, SigningAvailability};
use crate::SignerRegistration;

/// Every signing capability the engine thread currently holds, keyed by its
/// own public key. `Effect::RequestSign` resolves the exact pubkey frozen in
/// the accepted template; mutable current-account state can never redirect
/// already-accepted work.
/// #704: an idempotent cancel action for one outstanding remote-signer write
/// wait. It wraps the op's `Canceller`; firing it wakes the awaiting async task
/// to a disconnected end and runs the adapter cancel hook once.
type PendingWriteCancel = Box<dyn Fn() + Send>;

#[derive(Default)]
pub(super) struct SignerRegistry {
    signers: HashMap<PublicKey, RegisteredSigner>,
    pending_writes: RefCell<HashMap<(ReceiptId, u64), PendingWriteCancel>>,
}

/// The engine thread's single owner for identity-session membership and
/// operational signing-provider availability. Every public session mutation is
/// one command turn against this value.
///
/// It deliberately stores no current-account field (#1657). `EngineCore` holds
/// the one copy, and every read here takes it as `core.active_pubkey()`. The
/// two used to be written by adjacent statements at six sites with nothing
/// typing the pairing, so deleting either half still compiled while the
/// sign-event author check read one copy and reactive re-rooting read the
/// other.
#[derive(Default)]
pub(super) struct RuntimeSessionState {
    signers: SignerRegistry,
    accounts: HashMap<PublicKey, Option<SessionProvider>>,
}

impl std::ops::Deref for RuntimeSessionState {
    type Target = SignerRegistry;

    fn deref(&self) -> &Self::Target {
        &self.signers
    }
}

impl std::ops::DerefMut for RuntimeSessionState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.signers
    }
}

pub struct RuntimeSessionExportSources {
    pub snapshot: SessionSnapshot,
    pub providers: Vec<(PublicKey, Arc<nmp_local_signer::LocalKeySigner>)>,
}

impl RuntimeSessionState {
    pub(super) fn contains_account(&self, public_key: PublicKey) -> bool {
        self.accounts.contains_key(&public_key)
    }

    /// Idempotently note this account's provider without disturbing which
    /// signing capability, if any, is currently registered for it.
    pub(super) fn note_account(
        &mut self,
        public_key: PublicKey,
        provider: Option<SessionProvider>,
    ) {
        self.accounts.insert(public_key, provider);
    }

    /// Ensure `public_key` has a membership entry, leaving an existing one
    /// untouched.
    pub(super) fn ensure_account(&mut self, public_key: PublicKey) {
        self.accounts.entry(public_key).or_insert(None);
    }

    /// Remove this account's membership entry. Returns whether it existed.
    pub(super) fn remove_account(&mut self, public_key: PublicKey) -> bool {
        self.accounts.remove(&public_key).is_some()
    }

    /// Drop every account and every signing capability together, as one
    /// call — the exact pairing `ClearSession` needs, so the two halves
    /// can't drift the way the current-account/selection copies used to
    /// (see the struct doc above).
    pub(super) fn clear(&mut self) -> Vec<(PublicKey, core::AuthCapabilityInstance)> {
        let removed = self.signers.drain_instances();
        self.accounts.clear();
        removed
    }

    /// `current` is the reducer's one copy, passed in rather than stored
    /// (#1657); this value owns the account set, not the selection into it.
    pub(super) fn snapshot(&self, current: Option<PublicKey>) -> SessionSnapshot {
        let mut accounts = self
            .accounts
            .iter()
            .map(|(public_key, provider)| SessionAccount {
                public_key: *public_key,
                provider: *provider,
                signing: match provider {
                    None => SigningAvailability::Unsupported,
                    Some(SessionProvider::LocalKey) if self.has_local_provider(*public_key) => {
                        SigningAvailability::Available
                    }
                    Some(SessionProvider::LocalKey) => SigningAvailability::Unavailable {
                        reason: "configured signing provider is currently unavailable".to_string(),
                    },
                },
            })
            .collect::<Vec<_>>();
        accounts.sort_by_key(|account| account.public_key.to_bytes());
        SessionSnapshot {
            accounts,
            current_pubkey: current,
        }
    }

    pub(super) fn export_sources(&self, current: Option<PublicKey>) -> RuntimeSessionExportSources {
        let providers = self
            .accounts
            .iter()
            .filter_map(|(public_key, provider)| match provider {
                Some(SessionProvider::LocalKey) => self
                    .local_provider(*public_key)
                    .map(|provider| (*public_key, provider)),
                None => None,
            })
            .collect();
        RuntimeSessionExportSources {
            snapshot: self.snapshot(current),
            providers,
        }
    }

    pub(super) fn provider_pubkeys(&self) -> Vec<PublicKey> {
        self.accounts
            .iter()
            .filter_map(|(public_key, provider)| provider.map(|_| *public_key))
            .collect()
    }

    /// The signing capabilities this session holds, named rather than reached
    /// through this value's `Deref`.
    ///
    /// The `Deref` below is convenience for code that already knows both
    /// halves are the same value. A caller that depends only on the SIGNER
    /// half — [`crate::sign_event::ActiveSignEvents::admit`] is the one that
    /// matters (#1628) — asks for it by name, so its dependency reads as a
    /// dependency instead of a coercion.
    pub(super) fn signer_registry(&self) -> &SignerRegistry {
        &self.signers
    }
}

pub(super) fn encode_unsigned_event(unsigned: &UnsignedEvent) -> SignerUnsignedEvent {
    SignerUnsignedEvent::new(
        SignerPublicKey::new(unsigned.pubkey.to_bytes()),
        unsigned.created_at.as_secs(),
        unsigned.kind.as_u16(),
        unsigned
            .tags
            .clone()
            .to_vec()
            .into_iter()
            .map(Tag::to_vec)
            .collect(),
        unsigned.content.clone(),
    )
}

pub(super) fn decode_signed_event(
    signed: SignerSignedEvent,
) -> Result<SignedEvent, nmp_signer::SignerError> {
    let SignerSignedEventParts {
        id,
        public_key,
        created_at,
        kind,
        tags,
        content,
        signature,
    } = signed.into_parts();
    let id = EventId::from_slice(&id).map_err(|error| {
        nmp_signer::SignerError::InvalidResponse(format!("invalid event id: {error}"))
    })?;
    let public_key = PublicKey::from_slice(public_key.as_bytes()).map_err(|error| {
        nmp_signer::SignerError::InvalidResponse(format!("invalid event public key: {error}"))
    })?;
    let tags = tags
        .into_iter()
        .map(Tag::parse)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            nmp_signer::SignerError::InvalidResponse(format!("invalid event tag: {error}"))
        })?;
    let signature =
        nostr::secp256k1::schnorr::Signature::from_slice(&signature).map_err(|error| {
            nmp_signer::SignerError::InvalidResponse(format!("invalid event signature: {error}"))
        })?;
    Ok(SignedEvent::new(
        id,
        public_key,
        Timestamp::from(created_at),
        nostr::Kind::from(kind),
        tags,
        content,
        signature,
    ))
}

struct RegisteredSigner {
    identity: Arc<()>,
    instance: core::AuthCapabilityInstance,
    signer: SharedSigner,
}

#[derive(Clone)]
pub(super) enum SharedSigner {
    Local(Arc<nmp_local_signer::LocalKeySigner>),
    Shared(Arc<dyn SigningCapability + Send + Sync>),
}

impl SharedSigner {
    pub(super) fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        match self {
            Self::Local(signer) => signer.sign(unsigned),
            Self::Shared(signer) => signer.sign(unsigned),
        }
    }

    fn is_available(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Shared(signer) => signer.is_available(),
        }
    }

    fn local_provider(&self) -> Option<Arc<nmp_local_signer::LocalKeySigner>> {
        match self {
            Self::Local(signer) => Some(Arc::clone(signer)),
            Self::Shared(_) => None,
        }
    }
}

impl SignerRegistry {
    pub(super) fn contains(&self, pk: PublicKey) -> bool {
        self.signers.contains_key(&pk)
    }

    pub(super) fn len(&self) -> usize {
        self.signers.len()
    }

    pub(super) fn track_pending_write(
        &self,
        id: ReceiptId,
        generation: u64,
        cancel: PendingWriteCancel,
    ) {
        if let Some(stale) = self
            .pending_writes
            .borrow_mut()
            .insert((id, generation), cancel)
        {
            stale();
        }
    }

    pub(super) fn finish_pending_write(&self, id: ReceiptId, generation: u64) {
        self.pending_writes.borrow_mut().remove(&(id, generation));
    }

    pub(super) fn cancel_pending_write(&self, id: ReceiptId) {
        let mut pending = self.pending_writes.borrow_mut();
        let keys = pending
            .keys()
            .filter(|(receipt, _)| *receipt == id)
            .copied()
            .collect::<Vec<_>>();
        for key in keys {
            if let Some(cancel) = pending.remove(&key) {
                cancel();
            }
        }
    }

    pub(super) fn cancel_all_pending_writes(&self) {
        for (_, cancel) in self.pending_writes.borrow_mut().drain() {
            cancel();
        }
    }

    /// Register `signer` under its own `public_key()`, replacing any prior
    /// capability already registered for that key.
    pub(super) fn add(
        &mut self,
        pk: PublicKey,
        instance: core::AuthCapabilityInstance,
        signer: Box<dyn SigningCapability + Send + Sync>,
    ) -> (SignerRegistration, Option<core::AuthCapabilityInstance>) {
        let identity = Arc::new(());
        let replaced = self
            .signers
            .insert(
                pk,
                RegisteredSigner {
                    identity: Arc::clone(&identity),
                    instance,
                    signer: SharedSigner::Shared(Arc::from(signer)),
                },
            )
            .map(|old| old.instance);
        (
            SignerRegistration {
                public_key: pk,
                identity,
                instance,
            },
            replaced,
        )
    }

    pub(super) fn add_local(
        &mut self,
        pk: PublicKey,
        instance: core::AuthCapabilityInstance,
        signer: nmp_local_signer::LocalKeySigner,
    ) -> (SignerRegistration, Option<core::AuthCapabilityInstance>) {
        let identity = Arc::new(());
        let replaced = self
            .signers
            .insert(
                pk,
                RegisteredSigner {
                    identity: Arc::clone(&identity),
                    instance,
                    signer: SharedSigner::Local(Arc::new(signer)),
                },
            )
            .map(|old| old.instance);
        (
            SignerRegistration {
                public_key: pk,
                identity,
                instance,
            },
            replaced,
        )
    }

    /// Remove only the capability installed by this exact registration.
    /// A stale remote session can therefore never detach a newer replacement
    /// for the same account.
    pub(super) fn remove(
        &mut self,
        registration: &SignerRegistration,
    ) -> Option<core::AuthCapabilityInstance> {
        let is_current = self
            .signers
            .get(&registration.public_key)
            .is_some_and(|current| {
                current.instance == registration.instance
                    && Arc::ptr_eq(&current.identity, &registration.identity)
            });
        if !is_current {
            return None;
        }
        self.signers
            .remove(&registration.public_key)
            .map(|entry| entry.instance)
    }

    /// Resolve the signer frozen into this exact accepted template. An
    /// account switch cannot redirect already-accepted work.
    pub(super) fn sign(&self, unsigned: UnsignedEvent) -> Option<SignerOp<SignerSignedEvent>> {
        self.signers
            .get(&unsigned.pubkey)
            .map(|entry| entry.signer.sign(encode_unsigned_event(&unsigned)))
    }

    pub(super) fn auth_snapshot(
        &self,
        pk: PublicKey,
    ) -> Option<(core::AuthCapabilityInstance, SharedSigner)> {
        self.signers
            .get(&pk)
            .map(|entry| (entry.instance, entry.signer.clone()))
    }

    pub(super) fn is_available(&self, pk: PublicKey) -> bool {
        self.signers
            .get(&pk)
            .is_some_and(|entry| entry.signer.is_available())
    }

    fn has_local_provider(&self, pk: PublicKey) -> bool {
        self.signers
            .get(&pk)
            .is_some_and(|entry| matches!(entry.signer, SharedSigner::Local(_)))
    }

    fn local_provider(&self, pk: PublicKey) -> Option<Arc<nmp_local_signer::LocalKeySigner>> {
        self.signers
            .get(&pk)
            .and_then(|entry| entry.signer.local_provider())
    }

    pub(super) fn remove_key(
        &mut self,
        public_key: PublicKey,
    ) -> Option<core::AuthCapabilityInstance> {
        self.signers.remove(&public_key).map(|entry| entry.instance)
    }

    fn drain_instances(&mut self) -> Vec<(PublicKey, core::AuthCapabilityInstance)> {
        self.signers
            .drain()
            .map(|(public_key, entry)| (public_key, entry.instance))
            .collect()
    }
}
