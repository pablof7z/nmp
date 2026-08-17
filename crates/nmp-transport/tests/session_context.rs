use std::sync::mpsc;

use nmp_grammar::{RelaySessionKey};
use nmp_transport::{Pool, PoolConfig, RelayOpenError};
use nostr::{Keys, RelayUrl};

fn test_verifier() -> nmp_transport::Verifier {
    nmp_transport::Verifier::new(
        nmp_transport::VerifyConfig::default(),
        std::sync::Arc::new(nmp_transport::NullKnownSig),
    )
    .expect("test verifier construction must succeed")
}

#[test]
fn physical_session_cap_counts_contexts_not_urls() {
    let (tx, _rx) = mpsc::channel();
    let pool = Pool::new(
        PoolConfig {
            max_relays: 2,
            ..PoolConfig::default()
        },
        test_verifier(),
        tx,
    )
    .unwrap();
    let relay = RelayUrl::parse("ws://127.0.0.1:9").unwrap();
    let unauthenticated = RelaySessionKey::unauthenticated(relay.clone());
    let a = RelaySessionKey::new(
        relay.clone(),
        Some(Keys::generate().public_key()),
    );
    let b = RelaySessionKey::new(relay, Some(Keys::generate().public_key()));

    let unauthenticated_handle = pool.ensure_session(&unauthenticated).unwrap();
    assert_eq!(pool.ensure_session(&unauthenticated).unwrap(), unauthenticated_handle);
    let a_handle = pool.ensure_session(&a).unwrap();
    assert_ne!(a_handle, unauthenticated_handle);
    assert_eq!(
        pool.ensure_session(&b),
        Err(RelayOpenError::AtCapacity { max_relays: 2 })
    );

    pool.shutdown();
}
