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
//! (`docs/design/routing-and-ownership.md` §3.2.1). This crate OWNS NO event
//! kinds and exports NO `claims()` -- kind:24242 stays owned by `nmp-blossom`
//! and kind:20 by `nmp-nip68`, exactly as `nmp-nip29` composes kind:10009
//! without claiming it. See the `ownership_audit` module below.
//!
//! The FFI/Swift/Kotlin projection of this seam is a SEPARATE later unit
//! (batched with the nip68 projection, compile-gated) -- see
//! `docs/known-gaps.md`.

mod compose;
mod prepare;
mod upload;

pub use compose::{compose_picture, ComposedImage, MediaComposeError, PicturePost};
pub use prepare::{prepare, PrepareError, PreparedUpload};
pub use upload::{MediaUploadError, UploadedAsset};

#[cfg(test)]
mod ownership_audit {
    //! #559 ownership audit: this crate is a COMPOSITION crate, so it
    //! deliberately exports no `claims()` of its own -- "composition does not
    //! transfer ownership" (`docs/design/routing-and-ownership.md` §3.2.1),
    //! exactly the `nmp-nip29` stance. The two kinds the seam composes stay
    //! owned EXCLUSIVELY upstream (24242 by `nmp-blossom`, 20 by
    //! `nmp-nip68`); nmp-media wraps their artifacts without defining any.
    //! The ABSENCE of a `claims()` export from this crate IS the proof it
    //! registers nothing -- a future claim would be a new, reviewable,
    //! additive export, and `nmp-audit` enrolls this crate as
    //! `DeclaresNoClaims`.

    #[test]
    fn nmp_media_claims_nothing_and_upstream_owners_keep_their_kinds() {
        // kind:24242 (the Blossom authorization) is owned exclusively by
        // nmp-blossom; the upload stage authorizes and uploads WITHOUT
        // claiming that schema.
        let blossom = nmp_blossom::claims();
        assert!(blossom
            .iter()
            .any(|claim| claim.scope.contains(24242) && claim.exclusive));

        // kind:20 (the NIP-68 picture-first event) is owned exclusively by
        // nmp-nip68; the compose stage assembles that draft WITHOUT claiming
        // the schema -- only the artifact owner may define the artifact.
        let nip68 = nmp_nip68::claims();
        assert!(nip68
            .iter()
            .any(|claim| claim.scope.contains(20) && claim.exclusive));

        // nmp-media exports no `claims()` function at all -- there is nothing
        // to assert about it here, and that absence is the whole point.
    }
}
