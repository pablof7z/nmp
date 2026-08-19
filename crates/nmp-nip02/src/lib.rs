//! NIP-02: the reactive kind:3 contact-list demand, the durable
//! follow/unfollow write door, the kind:3 materializer, and the following
//! observation -- all of it, in one crate. #1707 reversed #1143: `nmp` must
//! not contain a single NIP-02-specific line, so everything that gives a
//! kind:3 contact list or a follow/unfollow edit its meaning lives here, not
//! split across a package boundary by whether it happens to touch the
//! engine. `demand` needs only `nostr`/`nmp-grammar`; `writes`/`observe`
//! need `nmp`'s own engine surface (`WriteIntent`, `Row`, receipt custody,
//! live-query folding) to compose a durable operation -- both live in this
//! one crate regardless.
//!
//! The dependency runs `nmp-nip02 -> nmp`, the ordinary shape of a
//! capability crate composing the engine it runs against, not an inversion:
//! `nmp` never names `nmp-nip02` back. A direct-Rust app that wants NIP-02
//! names two crates instead of one -- the uniform two-crate cost every
//! capability now pays for direct-Rust reach; a projection layer would still
//! get it for free through the one staticlib.

mod demand;
mod observe;
mod writes;

pub use demand::current_account_demand;
pub use observe::{
    observe_following, observe_following_async, set_following, AsyncFollowObservation,
    FollowActionFailure, FollowAvailability, FollowObservation, FollowRelationship, FollowSnapshot,
};
pub use writes::{follow_capability, follow_writes, follows, FollowChange, FollowWrites};
