// The one declarative owner for app-facing engine construction defaults.
//
// Each owning layer includes this file and supplies a callback macro that
// projects the declaration into its local types. Numeric values therefore
// cannot drift between the transport/runtime defaults, the direct-Rust
// facade, and UniFFI's literal-only record attributes.
macro_rules! with_nmp_engine_config_defaults {
    ($apply:ident) => {
        $apply! {
            store_path = none,
            indexer_relays = empty_list,
            app_relays = empty_list,
            fallback_relays = empty_list,
            allowed_local_relay_hosts = empty_list,
            max_relays = 10,
            max_auth_capabilities = 64,
        }
    };
}
