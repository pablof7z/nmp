//! `nmp-blossom` -- opt-in Blossom (BUD-01/02/03/04/11/12) blob core
//! (#545 upload + #551 mirror/delete/list, epic #216 T15-A-BLOSSOM). Per
//! `docs/design/protocol-modules-and-composition.md` §3: "Blossom uploads
//! bytes, verifies them, and returns an asset reference" -- and blob
//! operation failure and Nostr publication failure are DISTINCT results,
//! which is why this crate's per-operation taxonomies ([`UploadError`],
//! [`MirrorError`], [`DeleteError`], [`ListError`]) exist apart from any
//! engine receipt stream, and apart from each other.
//!
//! Engine-free, kind-agnostic core discipline (the `nmp-nip29` template):
//! this crate NEVER signs -- the draft builders
//! ([`upload_authorization_draft`], [`delete_authorization_draft`],
//! [`list_authorization_draft`]) emit an [`nostr::UnsignedEvent`] and the
//! caller signs it with the existing `nmp-signer` machinery, because
//! signing and publishing are orthogonal stages (#47/#32) -- and it never
//! touches the engine: no router, resolver, store, or engine dependency.
//! Its HTTP client reimplements the engine's NIP-11 admission discipline
//! from `nmp-transport`'s public pure classifiers (see `client.rs`).
//!
//! This unit covers the BUD-11 verbs (upload, BUD-04 mirror, BUD-12
//! delete/list); the `get`/`media` endpoints, platform projection, and
//! the NIP-68/composition layers are tracked follow-ups under epic #216
//! (see `docs/known-gaps.md`).

mod auth;
mod client;
mod descriptor;
mod server_list;
mod sha256;

pub use auth::{
    delete_authorization_draft, list_authorization_draft, upload_authorization_draft,
    AuthDraftError, AuthValidationError, BlossomVerb, ExpectedAuthorization, SignedAuthorization,
};
pub use client::{
    BlossomClient, BlossomClientConfig, BlossomServerUrl, ClientBuildError, DeleteError, ListError,
    ListPage, MirrorError, ServerAdmission, ServerAdmissionRefusal, ServerCandidateEvidence,
    ServerCandidatePolicy, ServerCandidateSource, ServerUrlError, UploadError, VerifiedUpload,
    DEFAULT_MAX_LIST_RESPONSE_BYTES, DEFAULT_MAX_RESPONSE_BYTES, DEFAULT_REQUEST_DEADLINE,
};
pub use descriptor::{BlobDescriptor, DescriptorError, MAX_DESCRIPTOR_BYTES};
pub use server_list::{
    active_account_server_list_demand, decode_server_list, decode_server_list_from_raw_tags,
    MalformedServerEntry, MalformedServerEntryReason, UserServerList, USER_SERVER_LIST_KIND,
};
pub use sha256::{Sha256Hash, Sha256HexError};

use nmp_ownership::{KindClaim, KindScope, ModuleId};

/// The exact Blossom-owned schemas:
///
/// - kind 24242: BUD-11 HTTP authorization;
/// - kind 10063: BUD-03 user server list.
///
/// Both are exclusive. Neither overrides wire routing: 24242 travels as an
/// HTTP header rather than a relay event, while kind 10063 is acquired through
/// the ordinary author-outbox demand in [`active_account_server_list_demand`].
const CLAIMS: [KindClaim; 2] = [
    KindClaim {
        owner: ModuleId::new("blossom"),
        scope: KindScope::Kind(24242),
        exclusive: true,
        route_policy: None,
        discovery_ack: false,
    },
    KindClaim {
        owner: ModuleId::new("blossom"),
        scope: KindScope::Kind(USER_SERVER_LIST_KIND),
        exclusive: true,
        route_policy: None,
        // 10063 is in DiscoveryKinds (10000..=19999).
        discovery_ack: true,
    },
];

/// This crate's ownership declaration (`nmp-ownership`'s `KindClaim`
/// vocabulary), consumed by the `nmp-audit` workspace fold.
pub fn claims() -> Vec<KindClaim> {
    CLAIMS.to_vec()
}

#[cfg(test)]
mod ownership_audit {
    //! #545/#731 ownership audit: `nmp-blossom` exclusively owns the two
    //! exact schemas defined by Blossom itself.

    use nmp_ownership::{KindScope, ModuleId};

    #[test]
    fn blossom_exclusively_claims_only_24242_and_10063() {
        let claims = crate::claims();
        assert_eq!(claims.len(), 2);
        assert_eq!(claims[0].owner, ModuleId::new("blossom"));
        assert_eq!(claims[0].scope, KindScope::Kind(24242));
        assert!(claims[0].exclusive);
        assert!(claims[0].route_policy.is_none());
        assert!(!claims[0].discovery_ack);

        assert_eq!(claims[1].owner, ModuleId::new("blossom"));
        assert_eq!(
            claims[1].scope,
            KindScope::Kind(crate::USER_SERVER_LIST_KIND)
        );
        assert!(claims[1].exclusive);
        assert!(claims[1].route_policy.is_none());
        assert!(claims[1].discovery_ack);
    }
}
