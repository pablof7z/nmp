//! `routeclass_has_no_default_and_no_app_constructor` (#33 test
//! obligation). Integration tests under `tests/` compile as their own
//! crate linking `nmp-ownership` as an external dependency -- exactly
//! the "outside the defining crate" position `RouteClass`'s per-variant
//! `#[non_exhaustive]` invariant is about, so this file is itself proof
//! the type behaves as documented from an app's point of view.
//!
//! The actual compiler-checked enforcement is the two `compile_fail`
//! doctests on `RouteClass` in `src/route_class.rs` (construction --
//! which also covers *naming* a variant in a pattern, since each variant
//! is individually `#[non_exhaustive]`, not just the enum -- and
//! `Default`). `cargo test -p nmp-ownership` runs those doctests on
//! every invocation, same as this test.

use nmp_ownership::RouteClass;

/// The only thing an external crate can do with a `RouteClass` value
/// (once something eventually hands it one -- nothing does yet, that's
/// Unit F) is treat it opaquely: hold it, `Debug`-print it, compare it
/// against another value it was also handed, or match it with a bare `_`
/// wildcard. It cannot name `RouteClass::Automatic` (or any other
/// variant) to construct one OR to match against it -- see the
/// `compile_fail` doctest for the direct proof.
fn describe(class: &RouteClass) -> String {
    format!("{class:?}")
}

#[test]
fn routeclass_has_no_default_and_no_app_constructor() {
    // None of these compile from this external crate:
    //     let _ = RouteClass::Automatic;           // no app constructor
    //     let _: RouteClass = Default::default();   // no Default
    //     match some_class { RouteClass::Automatic => {}, _ => {} } // can't name a variant either
    let _: fn(&RouteClass) -> String = describe;
}
