//! Compile-only external-consumer closure proof for NIP-65 bootstrap.
//!
//! This crate's manifest names only `nmp` and `nmp-nip65`. If composing and
//! publishing the operation ever requires `nmp-grammar` or a transport/store
//! mechanism crate, this crate stops compiling.

use nmp::{Engine, PublicKey, ReceiptStream, RelayUrl};
use nmp_nip65::{
    publish_relay_list_bootstrap, BootstrapRelayList, BootstrapRelayListError, RelayListEntry,
    RelayUsage,
};

pub fn bootstrap_new_account(
    engine: &Engine,
    author: PublicKey,
    bootstrap_relay: RelayUrl,
    outbox_relay: RelayUrl,
) -> Result<ReceiptStream, BootstrapRelayListError> {
    let request = BootstrapRelayList::new(
        author,
        vec![bootstrap_relay],
        vec![RelayListEntry::new(outbox_relay, RelayUsage::ReadWrite)],
    )?;
    publish_relay_list_bootstrap(engine, request)
}
