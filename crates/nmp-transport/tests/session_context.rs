use std::sync::mpsc;

use nmp_grammar::{AccessContext, RelaySessionKey};
use nmp_transport::{Pool, PoolConfig, RelayOpenError};
use nostr::{Keys, RelayUrl};

#[test]
fn physical_session_cap_counts_contexts_not_urls() {
    let (tx, _rx) = mpsc::channel();
    let pool = Pool::new(
        PoolConfig {
            max_relays: 2,
            ..PoolConfig::default()
        },
        tx,
    )
    .unwrap();
    let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
    let public = RelaySessionKey::public(relay.clone());
    let a = RelaySessionKey::new(
        relay.clone(),
        AccessContext::Nip42(Keys::generate().public_key()),
    );
    let b = RelaySessionKey::new(relay, AccessContext::Nip42(Keys::generate().public_key()));

    let public_handle = pool.ensure_session(&public).unwrap();
    assert_eq!(pool.ensure_session(&public).unwrap(), public_handle);
    let a_handle = pool.ensure_session(&a).unwrap();
    assert_ne!(a_handle, public_handle);
    assert_eq!(
        pool.ensure_session(&b),
        Err(RelayOpenError::AtCapacity { max_relays: 2 })
    );

    pool.shutdown();
}
