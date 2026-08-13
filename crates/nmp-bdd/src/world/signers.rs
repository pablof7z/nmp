//! The capability plane: which keys this world can sign for, when they
//! answer, and who was asked.
//!
//! Separate from [`super::identity`] because it is a different question.
//! Identity is who a write publishes AS -- a decision the app makes and NMP
//! freezes. A capability is whether anything can currently produce that key's
//! signature, which is a fact about the world rather than about the write,
//! and which may arrive minutes after the write was accepted. The whole
//! subject of `features/identity/awaiting-signer.feature` lives in the gap
//! between the two, so the gap has to be representable here: a key can be
//! registered with a signer, registered with a signer that has not answered
//! yet, or registered with none at all.
//!
//! Two things exist here that nowhere else provides:
//!
//! - [`SignerGate`] -- a signer that is asked immediately and answers only
//!   when the scenario says so. A signer that is simply absent proves the
//!   park; a signer that is present, asked, and outstanding is the only way
//!   to observe the window between `Accepted` and `Signed`, and that window
//!   is exactly where an account switch would retarget a write whose
//!   identity was not pinned at acceptance.
//! - a PER-KEY ask counter. "Neither A nor B was asked to sign it" is a
//!   claim about which signer was approached, and the world's single global
//!   `signer_asked` total cannot answer it.

use std::collections::BTreeMap;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use nostr::{Keys, PublicKey};

use nmp_local_signer::LocalKeySigner;
use nmp_signer::{
    SignerOp, SignerPublicKey, SignerSignedEvent, SignerUnsignedEvent, SigningCapability,
};

use super::budgets::EVENTUALLY;
use super::NmpWorld;

/// A signer that is ASKED immediately and ANSWERS only when released.
pub(super) struct SignerGate {
    released: Mutex<bool>,
    changed: Condvar,
}

impl SignerGate {
    fn new() -> Self {
        Self {
            released: Mutex::new(false),
            changed: Condvar::new(),
        }
    }

    /// `When <somebody>'s signer answers` -- every outstanding and every
    /// future request through this gate resolves from here on.
    fn release(&self) {
        *self.released.lock().unwrap_or_else(|p| p.into_inner()) = true;
        self.changed.notify_all();
    }

    fn wait_released(&self) {
        let mut released = self.released.lock().unwrap_or_else(|p| p.into_inner());
        while !*released {
            released = self
                .changed
                .wait(released)
                .unwrap_or_else(|p| p.into_inner());
        }
    }
}

/// The real local signer, held behind a [`SignerGate`] and counted per key.
struct GatedSigner {
    keys: Keys,
    asked: Arc<Mutex<BTreeMap<PublicKey, usize>>>,
    gate: Arc<SignerGate>,
}

impl SigningCapability for GatedSigner {
    fn public_key(&self) -> Option<SignerPublicKey> {
        Some(SignerPublicKey::new(self.keys.public_key().to_bytes()))
    }

    fn sign(&self, unsigned: SignerUnsignedEvent) -> SignerOp<SignerSignedEvent> {
        NmpWorld::count_ask(&self.asked, self.keys.public_key());
        let (sender, op) = SignerOp::pending_channel();
        let keys = self.keys.clone();
        let gate = Arc::clone(&self.gate);
        std::thread::spawn(move || {
            gate.wait_released();
            let signer = LocalKeySigner::from_secret_bytes(keys.secret_key().as_secret_bytes())
                .expect("nmp-bdd: fixture keys are valid secp256k1 scalars");
            let result = match signer.sign(unsigned) {
                SignerOp::Ready(result) => result,
                SignerOp::Pending(pending) => pending.recv(),
            };
            let _ = sender.resolve(result);
        });
        op
    }
}

impl NmpWorld {
    /// `Given the podcast identity's signer is slow to answer` / `Given that
    /// account's signer is slow to answer`.
    pub fn signer_is_slow(&mut self, label: &str) {
        self.slow_signers.push(label.to_string());
    }

    /// `Given that account's signer is offline` -- present in the scenario's
    /// world, absent from the engine's. Nothing can sign for this key until
    /// something becomes available, which is what makes the pin observable across a
    /// restart.
    pub fn signer_is_offline(&mut self, label: &str) {
        self.identities_with_signers.retain(|l| l != label);
    }

    /// `When <somebody>'s signer answers` -- the outstanding request this
    /// scenario has been holding open resolves, with a real signature.
    pub fn release_signer(&mut self, label: &str) {
        let gate = self
            .signer_gates
            .get(label)
            .unwrap_or_else(|| panic!("nmp-bdd: {label:?} has no slow signer to release"));
        gate.release();
    }

    /// `When a NIP-46 signing provider for "<hex>" becomes available` -- a signing capability
    /// for exactly that key arriving after the write was already accepted
    /// and parked. What the park waits on is a CAPABILITY for one pubkey;
    /// which transport carries it is not something the write can observe,
    /// and this world becomes available the one it can drive in-process.
    pub async fn attach_signer_for(&mut self, label: &str) {
        self.ensure_started().await;
        let keys = self.person(label);
        let signer = self.counting_signer(&keys);
        self.handle()
            .add_signer(signer)
            .expect("BDD local signer always exposes its public key");
        self.identities_with_signers.push(label.to_string());
    }

    /// Every identity the scenario said has a signer, registered against the
    /// (possibly freshly reconstructed) engine through the same door an app
    /// would use.
    ///
    /// A gated signer where the scenario said that one is slow to answer, an
    /// ordinary counted one otherwise; both count per key. Registered
    /// concretely rather than through a shared boxed type, because
    /// `Box<dyn SigningCapability>` is not itself a `SigningCapability`.
    pub(super) fn register_identity_signers(&mut self) {
        for label in self.identities_with_signers.clone() {
            self.add_signer_for(&label);
        }
    }

    /// One identity's capability, attached through the door an app uses.
    /// Its own method because an identity may be registered AFTER the engine
    /// is running (`world::identity::register_identity_with_signer`), and
    /// re-running the whole pass would re-register every other one.
    pub(super) fn add_signer_for(&mut self, label: &str) {
        let keys = self.person(label);
        if self.slow_signers.iter().any(|slow| slow == label) {
            let gate = Arc::new(SignerGate::new());
            self.signer_gates
                .insert(label.to_string(), Arc::clone(&gate));
            self.handle()
                .add_signer(GatedSigner {
                    keys,
                    asked: Arc::clone(&self.signer_asked_by),
                    gate,
                })
                .expect("BDD signers always expose their public key");
        } else {
            let signer = self.counting_signer(&keys);
            self.handle()
                .add_signer(signer)
                .expect("BDD signers always expose their public key");
        }
    }

    /// Count one ask against `pubkey` -- shared by the ordinary counted
    /// signer and the gated one, so "who was asked" is one number wherever
    /// the ask came from.
    pub(super) fn count_ask(counter: &Arc<Mutex<BTreeMap<PublicKey, usize>>>, pubkey: PublicKey) {
        *counter
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .entry(pubkey)
            .or_insert(0) += 1;
    }

    /// `Then "<hex>" is never asked to sign it` / `Then neither "<a>" nor
    /// "<b>" is asked to sign it` -- a claim about WHICH signer was
    /// approached, so it reads the per-key counter rather than the global
    /// one.
    pub fn signer_ask_count_for(&mut self, label: &str) -> usize {
        // Give anything the engine was going to do a chance to happen: an
        // ask that has not been issued yet is not proof it never will be.
        self.settle_last_write();
        let pubkey = self.person(label).public_key();
        self.ask_count(pubkey)
    }

    /// `Then it was signed by that account's signer` / `... the podcast
    /// identity's signer` -- the capability for that key was actually
    /// invoked, which the signed bytes alone cannot say (a payload can also
    /// arrive already signed).
    pub fn signer_was_asked_for(&mut self, label: &str) -> bool {
        let pubkey = self.person(label).public_key();
        let deadline = Instant::now() + EVENTUALLY;
        loop {
            if self.ask_count(pubkey) > 0 {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn ask_count(&self, pubkey: PublicKey) -> usize {
        self.signer_asked_by
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(&pubkey)
            .copied()
            .unwrap_or(0)
    }
}
