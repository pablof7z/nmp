//! [`RouteClass`] -- typed route provenance (routing-and-ownership.md
//! §3.3).

/// Why a publish/read is routed where it's routed. **No `Default` impl,
/// no app-reachable constructor.**
///
/// Each variant carries its own `#[non_exhaustive]` (not just the enum),
/// which -- verified against rustc, not just recalled -- is what actually
/// blocks external construction: enum-level `#[non_exhaustive]` alone
/// only forces a wildcard arm on matches, it does NOT stop a crate from
/// writing `RouteClass::Automatic` directly (existing unit variants stay
/// constructible). Per-variant `#[non_exhaustive]` makes each variant
/// name itself unreachable outside `nmp-ownership` -- E0603 "private unit
/// variant" -- the same way a private field would be, so an external
/// crate can neither construct NOR name a variant in a pattern; only a
/// bare `_` wildcard arm compiles. Same-crate code (this module's own
/// tests) is unaffected either way.
///
/// That's a real restriction, not just symbolic: it means the routing
/// layer that eventually mints these values (Unit F, deliberately not
/// this crate -- see the crate root docs) cannot do so by writing a
/// variant literal from `nmp-router`. `nmp-ownership` will need to grow a
/// blessed, crate-owned classification API (e.g. a method that takes the
/// facts and returns an already-built `RouteClass`) rather than exposing
/// raw variant construction -- the same "route authority lives behind
/// `RoutePolicy`/`KindClaim`, never standalone" shape those two types
/// already use.
///
/// No external construction or naming (only `_` compiles in a match):
/// ```compile_fail
/// let _ = nmp_ownership::RouteClass::Automatic;
/// ```
///
/// No `Default`:
/// ```compile_fail
/// let _: nmp_ownership::RouteClass = Default::default();
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RouteClass {
    /// Default policy: author outbox + p-tag inboxes + app lanes.
    #[non_exhaustive]
    Automatic,
    /// A module-pinned host (NIP-29 group anchor).
    #[non_exhaustive]
    HostPinned,
    /// A verified private inbox route (NIP-17 kind:10050 resolution) --
    /// carries the NarrowOnly set; only narrowing exists.
    #[non_exhaustive]
    VerifiedPrivateInbox,
    /// Explicit tooling route (nmp-demo / diagnostics CLI). Feature-gated
    /// out of the app-facing SDK build.
    #[non_exhaustive]
    Manual,
    /// Re-broadcast of an event authored elsewhere (import/mirror tools).
    #[non_exhaustive]
    Imported,
    /// Diagnostic probes (capability probing, NIP-66-style checks).
    #[non_exhaustive]
    Diagnostic,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Same-crate code CAN construct and name every variant directly --
    /// per-variant `#[non_exhaustive]` only restricts crates outside
    /// `nmp-ownership`. This is the positive half of the invariant; the
    /// negative half (no external construction, no `Default`) is proven
    /// by the two `compile_fail` doctests on the type itself, which
    /// `cargo test -p nmp-ownership` runs on every invocation.
    #[test]
    fn route_class_variants_constructible_and_matchable_in_crate() {
        let classes = [
            RouteClass::Automatic,
            RouteClass::HostPinned,
            RouteClass::VerifiedPrivateInbox,
            RouteClass::Manual,
            RouteClass::Imported,
            RouteClass::Diagnostic,
        ];
        for class in classes {
            match class {
                RouteClass::Automatic => assert_eq!(class, RouteClass::Automatic),
                RouteClass::HostPinned => assert_eq!(class, RouteClass::HostPinned),
                RouteClass::VerifiedPrivateInbox => {
                    assert_eq!(class, RouteClass::VerifiedPrivateInbox)
                }
                RouteClass::Manual => assert_eq!(class, RouteClass::Manual),
                RouteClass::Imported => assert_eq!(class, RouteClass::Imported),
                RouteClass::Diagnostic => assert_eq!(class, RouteClass::Diagnostic),
            }
        }
    }
}
