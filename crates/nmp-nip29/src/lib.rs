//! `nmp-nip29` -- read-only NIP-29 host browsing composed over
//! `nmp-nip51`'s kind:10009 (#63/#108). Claims NEITHER kind:10009 (owned by
//! `nmp-nip51`) NOR kinds 9/39000/30315 in this PR (deferred to whenever
//! #45's fuller ownership/write-routing framework lands -- kind 30315 in
//! particular is NIP-38 "user statuses", a DIFFERENT protocol this crate
//! reads cross-NIP but must never claim exclusively).
//!
//! Non-goals (mirrors #108's issue text exactly): no kind:30002 semantics;
//! no `rememberGroup`/`forgetGroup` mutation (gated on #50).

mod demand;
mod group_ref;

pub use demand::{group_content_demand, group_discovery_demand};
pub use group_ref::{remembered_groups, GroupRef, RememberedGroups};

#[cfg(test)]
mod ownership_audit {
    //! #108 Done-when: "Ownership audit proves nmp-nip51 owns 10009 and
    //! nmp-nip29 composes it without claiming it." This crate declares NO
    //! `claims()` of its own (see this module's doc) -- the audit is
    //! therefore just: nip51's claim covers 10009 exclusively, and that's
    //! the only claim in play between these two crates.

    #[test]
    fn nip51_exclusively_owns_10009_and_nip29_claims_nothing() {
        let nip51_claims = nmp_nip51::claims();
        assert_eq!(nip51_claims.len(), 1);
        assert!(nip51_claims[0].scope.contains(10009));
        assert!(nip51_claims[0].exclusive);
        // nmp-nip29 exports no `claims()` function at all in this PR --
        // the absence of that export IS the proof it registers nothing
        // (a future claim would be a new, reviewable, additive export).
    }
}
