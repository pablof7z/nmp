//! `nmp-nip22` -- the opt-in NIP-22 typed-comments-over-NIP-73-targets
//! protocol crate (#572), on the `nmp-nip68`/`nmp-nip29` template: exclusive
//! `KindClaim` on kind 1111, enrolled in `nmp-audit`, zero core/engine/store
//! changes. Core stays content-agnostic; this module owns kind:1111's exact
//! schema, semantic root/parent validation, immutable draft construction,
//! and root-thread demand -- never a UI, ranking, moderation, or product
//! policy.
//!
//! Same discipline as `nmp-nip29`/`nmp-nip68`/`nmp-blossom`: this crate
//! NEVER signs (`build`/`intent` emit an [`nostr::UnsignedEvent`]/
//! `WriteIntent` for the caller's own signer machinery) and NEVER touches
//! the engine -- `author`/`created_at` are explicit caller-supplied
//! parameters (this issue's own design decision), so there is no
//! `active_account()` query or wall-clock read anywhere in this crate, and
//! therefore no `engine` feature at all (unlike `nmp-nip29`, which needs
//! one for its semantic kind:9 operation).

mod build;
mod decode;
mod demand;
mod intent;
mod root;
mod target;

pub use build::{compose_comment_reply, compose_top_level_comment};
pub use decode::{decode_comment, CommentDecodeError, DecodedComment};
pub use demand::comment_thread_demand;
pub use intent::comment_intent;
pub use root::{CommentParent, CommentRoot, COMMENT_KIND};
pub use target::{Nip73Target, Nip73TargetError};

use nmp_ownership::{KindClaim, KindScope, ModuleId};

/// `nmp-nip22`'s one and only claim: kind:1111 (NIP-22 comment), exclusive.
/// `route_policy: None` -- kind:1111 travels on the ordinary author outbox
/// routing, so there is no routing authority to attach. Kind 1111 shares no
/// kind with `DiscoveryKinds` ({0, 3} ∪ 10000..=19999), so
/// `discovery_ack: false`.
const CLAIMS: [KindClaim; 1] = [KindClaim {
    owner: ModuleId::new("nip22"),
    scope: KindScope::Kind(COMMENT_KIND),
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
    //! #572 ownership audit: `nmp-nip22` claims EXACTLY the single kind
    //! 1111, exclusively, and nothing else.

    use nmp_ownership::{KindScope, ModuleId};

    #[test]
    fn nip22_exclusively_claims_kind_1111_and_nothing_else() {
        let claims = crate::claims();
        assert_eq!(claims.len(), 1);
        let claim = &claims[0];
        assert_eq!(claim.owner, ModuleId::new("nip22"));
        assert!(claim.exclusive);
        assert!(claim.scope.contains(1111));
        assert_eq!(claim.scope, KindScope::Kind(1111));
        assert!(claim.route_policy.is_none());
        assert!(!claim.discovery_ack);
    }
}
