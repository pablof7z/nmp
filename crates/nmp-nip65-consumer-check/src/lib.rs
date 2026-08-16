//! Compile-only external-consumer closure proof for NIP-65 bootstrap.
//!
//! This crate's manifest names only `nmp` and `nmp-nip65`. If composing and
//! publishing the operation ever requires `nmp-grammar` or a transport/store
//! mechanism crate, this crate stops compiling.

use nmp::{Engine, EngineError, PublicKey, ReceiptStream, RelayUrl};
use nmp_nip65::{BootstrapRelayList, BootstrapRelayListError, RelayListEntry, RelayUsage};

pub fn bootstrap_new_account(
    engine: &Engine,
    author: PublicKey,
    bootstrap_relay: RelayUrl,
    outbox_relay: RelayUrl,
) -> Result<Result<ReceiptStream, EngineError>, BootstrapRelayListError> {
    let request = BootstrapRelayList::new(
        author,
        vec![bootstrap_relay],
        vec![RelayListEntry::new(outbox_relay, RelayUsage::ReadWrite)],
    )?;
    // #1707: `Engine::publish_relay_list_bootstrap` is deleted -- it was
    // capability convenience (`engine.publish(request.into_write_intent())`,
    // the exact statement below), the same shape every other deleted door
    // was. An app composes the intent itself and publishes it like any
    // other write.
    Ok(engine.publish(request.into_write_intent()))
}
