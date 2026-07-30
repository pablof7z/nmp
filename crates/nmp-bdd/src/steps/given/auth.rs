use cucumber::given;

use crate::world::NmpWorld;

#[given(regex = r#"^relay "([^"]+)" requires authentication for writes$"#)]
async fn relay_requires_write_auth(w: &mut NmpWorld, relay: String) {
    w.require_write_auth(&relay);
}

#[given(regex = r#"^my authentication policy denies "([^"]+)" with "([^"]+)"$"#)]
async fn auth_policy_denies_relay(w: &mut NmpWorld, relay: String, reason: String) {
    w.deny_write_auth_by_policy(&relay, &reason);
}
