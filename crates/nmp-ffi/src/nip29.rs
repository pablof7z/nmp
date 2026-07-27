//! Read-only NIP-29 group-discovery projection (#108/#838): one top-level
//! free function, same shape as [`crate::entity`]'s precedent (#116).
//!
//! #858: nothing here re-labels NIP-51's value. The kind:10009 Simple-groups
//! list is decoded ONCE, as itself, by [`crate::nip51`]; a native caller
//! selects one [`crate::types::FfiSimpleGroupEntry`] and hands its exact
//! `host_relay` field to group discovery itself. This module exports no
//! NIP-51 record type and no decode door of its own.
//!
//! The selected host rides as `SourceAuthority::Pinned({host})` on the
//! returned `FfiDemand` -- pass it straight to
//! `NmpEngine::observe_demand`, exactly like any other `FfiDemand` (#107).
//! No new subscribe verb exists or is needed for this feature.
//!
//! Deliberately absent: a fixed group-content kind catalog and a kind:9
//! composer. NIP-29 owns neither; C7 and client notification policy remain
//! independently optional (#838).

use nostr::RelayUrl;

use crate::convert::{demand_to_ffi, FfiError};
use crate::types::FfiDemand;

fn parse_host(host: String) -> Result<RelayUrl, FfiError> {
    RelayUrl::parse(&host).map_err(|_| FfiError::InvalidRelayUrl { got: host })
}

/// Group discovery (kind:39000) pinned to `host` (#108,
/// `nmp_nip29::group_discovery_demand` mirror). `host` crosses the FFI
/// boundary as a raw string, unlike the direct-Rust constructor's
/// `RelayUrl` -- fallibility is restored HERE (an FFI caller can supply a
/// malformed URL the direct-Rust singleton-set proof doesn't cover).
#[uniffi::export]
pub fn group_discovery_demand(host: String) -> Result<FfiDemand, FfiError> {
    Ok(demand_to_ffi(nmp_nip29::group_discovery_demand(
        parse_host(host)?,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::FfiSourceAuthority;

    #[test]
    fn group_discovery_demand_pins_a_parsed_host() {
        let demand = group_discovery_demand("wss://host-1.example.com".to_string())
            .expect("well-formed host url");
        assert_eq!(demand.selection.kinds, Some(vec![39000]));
        match demand.source {
            FfiSourceAuthority::Pinned { relays } => {
                assert_eq!(relays, vec!["wss://host-1.example.com".to_string()]);
            }
            other => panic!("expected Pinned, got {other:?}"),
        }
    }

    #[test]
    fn group_discovery_demand_rejects_an_unparseable_host() {
        match group_discovery_demand("not-a-url".to_string()) {
            Err(FfiError::InvalidRelayUrl { got }) => assert_eq!(got, "not-a-url"),
            other => panic!("expected InvalidRelayUrl, got {other:?}"),
        }
    }
}
