//! `Given` — the store already holds a stated winner at a replaceable
//! coordinate, before the scenario's app acts.
//!
//! Its own file next to `when::writes` for the reason `then/` is a
//! directory: this family's step is shared Background vocabulary every
//! `features/writes/` scenario that needs a pre-existing winner uses.

use cucumber::given;

use crate::world::{parse_stated_time, NmpWorld};

// Stated as a `Given` because it is the world before the app acts, even
// though it really does publish: see `world::replaceable` for why. Real
// consumers: every scenario in `features/writes/replaceable-operations.feature`
// and `features/diagnostics/stalled-writes.feature` that starts from an
// existing winner.
#[given(regex = r#"^my contact list "([0-9a-f]{64})" created at "([^"]+)" is the stored winner$"#)]
async fn my_contact_list_is_the_winner(w: &mut NmpWorld, label: String, at: String) {
    let me = w.current_identity();
    w.stage_stored_winner(&me, &label, parse_stated_time(&at))
        .await;
}
