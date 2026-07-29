//! Plain name-to-fixture facts about the staged world.
//!
//! These are immediate lookups, not observations: they never fold a channel,
//! wait for a delivery, or interpret engine output. Keeping them separate
//! leaves `observe` responsible only for accumulated runtime evidence and
//! gives step/world helpers one obvious place to resolve scenario names.

use nmp_router::RelayUrl;

use super::NmpWorld;

impl NmpWorld {
    pub fn indexer_names(&self) -> &[String] {
        &self.indexer_names
    }

    pub fn relay_names(&self) -> impl Iterator<Item = &String> {
        self.relay_order.iter()
    }

    pub fn relay_url(&self, name: &str) -> RelayUrl {
        self.relays
            .get(name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {name:?}"))
            .url
            .clone()
    }

    pub fn write_relay_of(&self, person: &str) -> Vec<String> {
        self.write_relay_of.get(person).cloned().unwrap_or_default()
    }

    pub fn pubkey_hex(&self, person: &str) -> String {
        self.people
            .get(person)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown person {person:?}"))
            .public_key()
            .to_hex()
    }
}
