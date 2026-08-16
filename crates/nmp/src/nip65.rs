//! Optional Rust NIP-65 facade door.
//!
//! Protocol values stay engine-free in `nmp-nip65`; this module exposes what
//! an app names and binds the one bootstrap publish to the facade.
//!
//! The runtime glue that drives the coordinator is deliberately NOT here. It
//! lives in `nmp_runtime::nip65`, beside the loop that calls it, so the
//! engine never depends upward on a facade module. See that module's header.

use crate::{Engine, EngineError, ReceiptStream};

pub use nmp_nip65::{
    relay_list_demand, BootstrapRelayList, BootstrapRelayListError, CoordinatorQuery,
    CoordinatorUpdate, ParsedAuthorRoutes, RelayListEntry, RelayUsage, RELAY_LIST_KIND,
};

impl Engine {
    /// Engine binding for the pure bootstrap value.
    pub fn publish_relay_list_bootstrap(
        &self,
        request: BootstrapRelayList,
    ) -> Result<ReceiptStream, EngineError> {
        self.publish(request.into_write_intent())
    }
}
