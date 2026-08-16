//! The one assertion shared with a whole-value replacement's Background:
//! whether the accepted write went through.
//!
//! A different domain from [`super::payloads`], which is about the event a
//! publish carried, and from [`super::writes`], which is about where it
//! went.

use cucumber::then;

use crate::world::NmpWorld;

/// Also read by `features/diagnostics/stalled-writes.feature`, whose subject
/// is a write nothing can deliver rather than a replacement: "the obligation
/// was accepted" is the same observable in both, and `publish()` answering
/// `Ok` is what both mean. Narrowing this to something replaceable-specific
/// would silently break that feature, so it stays the plain acceptance
/// answer.
#[then(regex = r#"^the write is accepted$"#)]
async fn the_write_is_accepted(w: &mut NmpWorld) {
    assert!(
        w.replacement_accepted(),
        "expected the replacement to be accepted; saw {:?}",
        w.receipt_statuses()
    );
}
