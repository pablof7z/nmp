//! `nmp-nip68` -- the opt-in NIP-68 picture-first (kind:20) protocol crate
//! (#558, epic #216 T15-B-NIP68-IMETA).
//!
//! Per `docs/design/protocol-modules-and-composition.md` §3, this crate OWNS
//! the NIP-68 photo event schema exclusively: "Composition does not transfer
//! ownership: a context owner may wrap an artifact, but only the artifact owner
//! may define the artifact." It builds an immutable UNSIGNED kind:20 draft from
//! content-addressed image artifacts (the Blossom [`nmp_blossom::BlobDescriptor`]
//! seam) and decodes a kind:20 event into typed picture facts.
//!
//! Same discipline as `nmp-nip29`/`nmp-blossom`: this crate NEVER signs (it
//! emits an [`nostr::UnsignedEvent`] for the caller's existing `nmp-signer`
//! machinery -- signing and publishing are orthogonal stages, #47/#32) and
//! NEVER touches the engine (no router/resolver/store/engine dependency).
//!
//! Artifact provenance is STRUCTURAL: [`PictureImage`] carries `url`/`m`/`x` by
//! construction (private fields, provenance-only constructors), a descriptor
//! without a mime type cannot mint one, and a spec with zero images is refused
//! -- the #421 "protected kind without artifact provenance fails" contract.
//!
//! FFI/Swift/Kotlin projection and the T15-C upload->build->sign->publish
//! composition seam (#559) are SEPARATE later units -- see `docs/known-gaps.md`.

mod build;
mod decode;
mod image;

pub use build::{build_picture, ContentWarning, PictureBuildError, PictureSpec, PICTURE_KIND};
pub use decode::{
    decode_picture, decode_picture_from_raw, DecodedImage, Picture, PictureDiagnostic,
};
pub use image::{ImageDim, ImageDimError, PictureImage, PictureImageError};

use nmp_ownership::{KindClaim, KindScope, ModuleId};

/// `nmp-nip68`'s one and only claim: kind:20 (NIP-68 picture-first event),
/// exclusive. `route_policy: None` -- kind:20 travels on the ordinary author
/// outbox routing, so there is no routing authority to attach. Kind 20 shares
/// no kind with `DiscoveryKinds` ({0, 3} ∪ 10000..=19999), so `discovery_ack:
/// false`.
const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("nip68"),
    scope: KindScope::Kind(20),
    exclusive: true,
    route_policy: None,
    discovery_ack: false,
}];

/// This crate's ownership declaration (`nmp-ownership`'s `KindClaim`
/// vocabulary), consumed by the `nmp-audit` workspace fold.
pub fn claims() -> Vec<KindClaim> {
    CLAIMS.to_vec()
}

#[cfg(test)]
mod ownership_audit {
    //! #558 ownership audit: `nmp-nip68` claims EXACTLY the single kind 20,
    //! exclusively, and nothing else -- the NIP-68 picture-first event is the
    //! only Nostr artifact this crate defines.

    use nmp_ownership::{KindScope, ModuleId};

    #[test]
    fn nip68_exclusively_claims_kind_20_and_nothing_else() {
        let claims = crate::claims();
        assert_eq!(claims.len(), 1);
        let claim = &claims[0];
        assert_eq!(claim.owner, ModuleId::new("nip68"));
        assert!(claim.exclusive);
        assert!(claim.scope.contains(20));
        // "and nothing else": the scope is the single kind, not a range or set
        // that could quietly grow.
        assert_eq!(claim.scope, KindScope::Kind(20));
        assert!(claim.route_policy.is_none());
        // Kind 20 is not a discovery kind ({0, 3} ∪ 10000..=19999).
        assert!(!claim.discovery_ack);
    }
}
