//! `nmp-blossom` -- opt-in Blossom (BUD-01/02/11) blob-upload core
//! (#545, epic #216 T15-A-BLOSSOM). Per
//! `docs/design/protocol-modules-and-composition.md` §3: "Blossom uploads
//! bytes, verifies them, and returns an asset reference" -- and upload
//! failure and Nostr publication failure are DISTINCT results, which is
//! why this crate's [`UploadError`] taxonomy exists apart from any
//! engine receipt stream.
//!
//! Engine-free, kind-agnostic core discipline (the `nmp-nip29` template):
//! this crate NEVER signs -- [`upload_authorization_draft`] emits an
//! [`nostr::UnsignedEvent`] and the caller signs it with the existing
//! `nmp-signer` machinery, because signing and publishing are orthogonal
//! stages (#47/#32) -- and it never touches the engine: no router,
//! resolver, store, or engine dependency. Its HTTP client reimplements
//! the engine's NIP-11 admission discipline from `nmp-transport`'s public
//! pure classifiers (see `client.rs`).
//!
//! This unit covers BUD-02 upload only; mirror/delete/list, platform
//! projection, and the NIP-68/composition layers are tracked follow-ups
//! under epic #216 (see `docs/known-gaps.md`).

mod auth;
mod client;
mod descriptor;
mod sha256;

pub use auth::{
    upload_authorization_draft, AuthDraftError, AuthValidationError, BlossomVerb,
    ExpectedAuthorization, SignedAuthorization,
};
pub use client::{
    BlossomClient, BlossomClientConfig, BlossomServerUrl, ClientBuildError, ServerUrlError,
    UploadError, VerifiedUpload, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_REQUEST_DEADLINE,
};
pub use descriptor::{BlobDescriptor, DescriptorError, MAX_DESCRIPTOR_BYTES};
pub use sha256::{Sha256Hash, Sha256HexError};

use nmp_ownership::{KindClaim, KindScope, ModuleId};

/// `nmp-blossom`'s one and only claim: kind 24242 (Blossom BUD-11
/// authorization event construction and validation), exclusive.
/// `route_policy: None` -- authorization events travel as HTTP headers,
/// never through wire routing, so there is no routing authority to
/// attach. 24242 shares no kind with `DiscoveryKinds` ({0, 3} ∪
/// 10000..=19999) -- nothing to consciously acknowledge.
const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("blossom"),
    scope: KindScope::Kind(24242),
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
    //! #545 ownership audit: `nmp-blossom` claims EXACTLY the single kind
    //! 24242, exclusively, and nothing else -- the BUD-11 authorization
    //! event is the only Nostr artifact this crate constructs.

    use nmp_ownership::{KindScope, ModuleId};

    #[test]
    fn blossom_exclusively_claims_24242_and_nothing_else() {
        let claims = crate::claims();
        assert_eq!(claims.len(), 1);
        let claim = &claims[0];
        assert_eq!(claim.owner, ModuleId::new("blossom"));
        assert!(claim.exclusive);
        assert!(claim.scope.contains(24242));
        // "and nothing else": the scope is the single kind, not a range or
        // set that could quietly grow.
        assert_eq!(claim.scope, KindScope::Kind(24242));
        assert!(claim.route_policy.is_none());
        // 24242 is not a discovery kind ({0, 3} ∪ 10000..=19999).
        assert!(!claim.discovery_ack);
    }
}
