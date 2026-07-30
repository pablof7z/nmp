//! `Then` — an observable outcome, always one of the four channels
//! (approach doc §1.3): rows on a feed, receipt states, diagnostics facts,
//! acquisition-evidence facts. Every assertion below reads ONLY through
//! `NmpWorld`'s public observers (`feed_*`/`receipt_*`/`diagnostics_*`/
//! `relay_contacted`/`relay_untouched_since_snapshot`) -- never anything
//! engine-internal.
//!
//! # The empty-world rule: a step that cannot fail is not coverage
//!
//! Ask of every assertion here: IF THE WORLD PRODUCED NOTHING AT ALL, DOES
//! THIS STEP STILL PASS? A loop over an empty collection, an `.all()` over
//! nothing, a `difference()` from an empty wanted-set, a count that is zero
//! because the engine never ran -- each of those is green, and green for the
//! same reason a correct implementation is green. A scenario whose `Given`
//! is incomplete then reads exactly like a scenario that proves something:
//! four `features/routing/bounded-feed-window.feature` scenarios were once
//! written without `Given my relay list names ... as my write relay`, so the
//! kind:3 follow list was never discoverable, no REQ ever reached the wire,
//! and every assertion behaved identically with and without the fix they
//! existed to test.
//!
//! So a step must establish that there was something to observe BEFORE it
//! asserts anything about it, through [`nothing_to_observe`] -- whose message
//! names WHAT WAS MISSING and is deliberately worded unlike a failed
//! assertion, so the two classes are distinguishable at a glance in suite
//! output and `NOTHING TO OBSERVE` greps for exactly the scenarios that
//! proved nothing.
//!
//! # How the families below are split
//!
//! BY THE DOMAIN THE CLAIM IS ABOUT, not by the channel it happens to read.
//! That matters because the same channel serves very different claims --
//! diagnostics carries both "the indexers were only asked for discovery
//! kinds" (a routing claim) and "this relay refused two subscriptions" (a
//! budget claim) -- and it is the domain, not the channel, that decides
//! whether a new assertion belongs with an existing family or is a new one.
//! Each file below also owns the private decoding helpers its own family
//! needs, so a helper never outlives the claims it was written for.
//!
//! - `feed` -- what the app-visible feed shows, and what it must never show.
//! - `identity` -- WHO a write published as: the account it resolved to, the
//!   key it stayed pinned to, and what a named key with no signer does
//!   instead of failing. Distinct from `writes`; both read the receipt, only
//!   one is about authorship.
//! - `groups` -- the NIP-29 `Group` door: which HOST an event reached (and
//!   which hosts it did not), what the delivered event literally was, and
//!   what the door refused. Distinct from `writes` again: that family asks
//!   where a publish was routed, this one asks a narrower and stricter
//!   question and answers it from the relay's own record of the bytes as
//!   well as from the receipt.
//! - `writes` -- the write plane: where a publish was routed, what its
//!   receipt said, and that a republished payload came back out untouched.
//! - `payloads` -- the EVENT rather than the write: what NMP filled in on a
//!   builder that said nothing, what it left alone when the app did say
//!   something, and what an already-signed event's bytes still were on the
//!   far side. Distinct from `writes` and `identity` for the same reason
//!   those two are distinct from each other -- same receipt, different claim.
//! - `outbox` -- the DEFAULT route: which relays an ordinary `Auto` write
//!   resolved to, and what the engine said when it resolved to none. A third
//!   question from `writes` ("where was it delivered") and `routes` ("is it
//!   still deciding"), and the only one of the three that can tell an outbox
//!   consulting the wrong half of a relay list from one consulting the right
//!   half and failing to reach it.
//! - `routes` -- routing as a LIFECYCLE: whether the strategy is still
//!   deciding, what it says it is waiting for, and whether it can ever change
//!   its mind again. Separate from `writes` because routed and published are
//!   separate axes, and a suite that read one off the other could not tell a
//!   misconfigured indexer set from a slow relay.
//! - `provenance` -- WHO DELIVERED each row the feed shows. Distinct from
//!   `feed` for the same reason `payloads` is distinct from `writes`: same
//!   channel, different claim, and the two answer independently.
//! - `routing` -- the READ plane: which relay was asked for what kind, in
//!   which lane. Distinct from `writes`; both talk about relays, only one is
//!   about an event this app sent.
//! - `stalled` -- the engine-global "is anything quietly stuck" list. A
//!   different domain from `routes`/`writes` even though all three describe
//!   the write plane: those read a RECEIPT, and every claim here is made by
//!   an app holding none.
//! - `wire` -- the REQ/CLOSE frames NMP actually put on a relay socket.
//! - `budget` -- what a relay says it can hold, and what happened when it
//!   could not hold it.
//!
//! The empty-world rule applies to every family below, so
//! [`nothing_to_observe`] is defined HERE, textually BEFORE the module
//! declarations below: a `macro_rules!` is in scope for every module declared
//! after it, which is what lets one definition serve every family without
//! exporting anything from the crate. A new family goes below the macro for
//! the same reason.

/// A step's precondition that the world produced the thing it reads (see this
/// module's doc). `$present` is the PRECONDITION -- true when there is
/// something to observe -- and the message names what was missing when there
/// is not. The shared tail lives here so the phrasing is identical
/// everywhere and the class is greppable by its `NOTHING TO OBSERVE` prefix.
macro_rules! nothing_to_observe {
    ($present:expr, $($missing:tt)+) => {
        assert!(
            $present,
            "NOTHING TO OBSERVE -- {} -- so this step reads an empty world and \
             would pass whatever the engine did; a check that cannot fail is not \
             coverage, and the scenario's setup is what needs fixing",
            format_args!($($missing)+)
        )
    };
}

mod budget;
mod feed;
mod groups;
mod identity;
mod outbox;
mod payloads;
mod provenance;
mod replaceable;
mod routes;
mod routing;
mod stalled;
mod wire;
mod writes;
