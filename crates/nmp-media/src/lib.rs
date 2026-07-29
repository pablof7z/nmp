//! `nmp-media` -- the opt-in, app-facing STAGED composition seam that turns
//! raw image bytes into a publishable NIP-68 kind:20 draft (#559, epic #216
//! T15-C-MEDIA-COMPOSITION). Per
//! `docs/design/protocol-modules-and-composition.md` §3 and `docs/VISION.md`
//! §4, the pipeline is:
//!
//! ```text
//! asset   = Blossom.upload(file)     // standalone async HTTP -> VerifiedUpload
//! photo   = Nip68.buildPhoto(asset)  // kind:20 UnsignedEvent
//! receipt = publish(photo)           // EXISTING WriteIntent path -- NOT built here
//! ```
//!
//! This crate makes that pipeline witness-typed:
//! `Sha256Hash -> signed authorization -> VerifiedUpload -> kind:20 draft`,
//! so a SKIPPED or FAILED stage is UNREPRESENTABLE. Its real contribution
//! beyond calling the two upstream APIs separately is the [`prepare()`] safety
//! invariant: a [`PreparedUpload`] OWNS the exact bytes it hashed and
//! authorized, so it is structurally impossible to authorize the hash of
//! bytes A and then upload bytes B.
//!
//! # Scope (Option 1 -- standalone async upload, durable later)
//! @pablof7z's decision scopes this crate to the STANDALONE upload. The
//! engine-integrated DURABLE upload (persisted intent / reattachable receipt
//! / HTTP-publish Effect / blob persistence) is a SEPARATE, additive issue
//! (#562) whose witness types are identical to these. This crate therefore
//! does NOT touch the engine, the facade, the outbox, or the store, and does
//! NOT sign or publish -- signing happens UPSTREAM of the seam (the app signs
//! [`PreparedUpload::authorization_draft`] with `nmp-signer`) and
//! relay/publish happens DOWNSTREAM (the app hands the composed kind:20
//! [`nostr::UnsignedEvent`] to the existing `publish()` -> WriteIntent path).
//!
//! # Separated failure domains (§3 doctrine)
//! "Blossom upload failure and Nostr publication failure remain separate
//! outcomes." The three stages fail into three SEPARATE TYPES --
//! [`PrepareError`], [`MediaUploadError`], [`MediaComposeError`] -- never one
//! merged enum, so an upload failure can never be pattern-matched (or `?`-ed)
//! as a compose failure. [`MediaUploadError`] preserves the WHOLE Blossom
//! [`nmp_blossom::UploadError`] taxonomy inside one `Blossom(..)` variant
//! rather than re-collapsing it.
//!
//! # Ownership (composition is not schema ownership)
//! "Composition does not transfer ownership: a context owner may wrap an
//! artifact, but only the artifact owner may define the artifact"
//! (`docs/design/routing-and-ownership.md` §3.2.1). This crate defines NO
//! event schema of its own: kind:24242 is defined and parsed only by
//! `nmp-blossom` and kind:20 only by `nmp-nip68`. The structural proof is the
//! dependency direction -- nmp-media depends on those crates and neither
//! re-implements nor re-exports their builders/codecs.
//!
//! # Projection (#971)
//! The PREPARE + UPLOAD stages now have one supported native projection:
//! `nmp::Engine::upload_blossom` composes [`prepare`] and
//! [`PreparedUpload::upload`] with the governed engine signer into a single
//! call, projected through `nmp-ffi`/Swift/Kotlin. That operation is still the
//! STANDALONE upload described above -- it adds no durability, no receipt and
//! no scheduler, so #562 stays rejected. The COMPOSE stage's projection
//! remains a separate later unit -- see `docs/known-gaps.md`.

mod compose;
mod prepare;
mod upload;

pub use compose::{compose_picture, ComposedImage, MediaComposeError, PicturePost};
pub use prepare::{prepare, PrepareError, PreparedUpload};
pub use upload::{MediaUploadError, UploadedAsset};
