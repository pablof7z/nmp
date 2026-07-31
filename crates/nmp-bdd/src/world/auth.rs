//! Public AUTH-policy staging and receipt/socket observations.
//!
//! The positive fact comes from the facade receipt. The negative “never
//! attempted again” fact comes from the scripted relay's raw websocket tap,
//! so the engine is not allowed to corroborate its own claim.

use nmp::mechanism::delivery::{AuthDenialSource, WriteStatus};

use super::budgets::NEVER;
use super::{AuthDenialObservation, NmpWorld};

impl NmpWorld {
    /// Observe an exact policy denial, retaining its first live-process fact
    /// for comparison with replay after reconstruction.
    pub fn receipt_reports_policy_auth_denial(&mut self, relay_name: &str) -> bool {
        let relay = self.relay_url(relay_name);
        let matches = |seen: &[WriteStatus]| {
            seen.iter().any(|status| {
                matches!(
                    status,
                    WriteStatus::AuthDenied {
                        relay: denied_relay,
                        source: AuthDenialSource::Policy,
                        ..
                    } if *denied_relay == relay
                )
            })
        };
        let observed = if self.restarted_receipt.is_some() {
            self.restarted_receipt_eventually(matches)
        } else {
            self.receipt_eventually(matches)
        };
        if !observed {
            return false;
        }

        let statuses = if self.restarted_receipt.is_some() {
            self.restarted_receipt_statuses()
        } else {
            self.receipt_statuses()
        };
        let Some((pubkey, source, reason)) =
            statuses.iter().rev().find_map(|status| match status {
                WriteStatus::AuthDenied {
                    relay: denied_relay,
                    pubkey,
                    source,
                    reason,
                } if *denied_relay == relay && *source == AuthDenialSource::Policy => {
                    Some((*pubkey, *source, reason.clone()))
                }
                _ => None,
            })
        else {
            return false;
        };

        let expected_pubkey = self
            .active_person
            .as_ref()
            .and_then(|name| self.people.get(name))
            .map(nostr::Keys::public_key)
            .expect("nmp-bdd: an AUTH-policy write has an active account");
        if pubkey != expected_pubkey {
            return false;
        }

        let event_attempts = self
            .relays
            .get(relay_name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {relay_name:?}"))
            .wire_record()
            .event_ids
            .len();
        if event_attempts == 0 {
            return false;
        }

        let current = AuthDenialObservation {
            pubkey,
            source,
            reason,
            event_attempts,
        };
        match self.auth_denial_observations.get(relay_name) {
            Some(first) => {
                first.pubkey == current.pubkey
                    && first.source == current.source
                    && first.reason == current.reason
            }
            None => {
                self.auth_denial_observations
                    .insert(relay_name.to_string(), current);
                true
            }
        }
    }

    /// The live or replayed policy denial kept the app's exact sentence.
    pub fn policy_auth_denial_reason_is(&mut self, relay_name: &str, expected: &str) -> bool {
        if !self.receipt_reports_policy_auth_denial(relay_name) {
            return false;
        }
        self.auth_denial_observations
            .get(relay_name)
            .is_some_and(|observed| observed.reason == expected)
    }

    pub fn any_policy_auth_denial_reason_is(&mut self, expected: &str) -> bool {
        let names: Vec<String> = self.auth_policy_denials.keys().cloned().collect();
        names
            .iter()
            .any(|name| self.policy_auth_denial_reason_is(name, expected))
    }

    /// Rebuilding the engine already reattached the receipt by id; this makes
    /// that app-visible boundary explicit in the scenario.
    pub fn restarted_receipt_is_reattached(&self) -> bool {
        self.restarted_receipt.is_some()
    }

    /// The replayed terminal fact is byte-for-byte the first live denial.
    pub fn replayed_auth_denial_matches_first(&mut self, relay_name: &str) -> bool {
        if self.restarted_receipt.is_none() {
            return false;
        }
        let Some(first) = self.auth_denial_observations.get(relay_name).cloned() else {
            return false;
        };
        if !self.receipt_reports_policy_auth_denial(relay_name) {
            return false;
        }
        self.auth_denial_observations
            .get(relay_name)
            .is_some_and(|replayed| {
                replayed.pubkey == first.pubkey
                    && replayed.source == first.source
                    && replayed.reason == first.reason
            })
    }

    pub fn any_replayed_auth_denial_matches_first(&mut self) -> bool {
        let names: Vec<String> = self.auth_denial_observations.keys().cloned().collect();
        names
            .iter()
            .any(|name| self.replayed_auth_denial_matches_first(name))
    }

    /// Spend the suite's full negative window, then compare the relay's raw
    /// EVENT-frame count with the one captured before process loss.
    pub async fn no_event_attempt_after_auth_denial(&self, relay_name: &str) -> bool {
        let Some(first) = self.auth_denial_observations.get(relay_name) else {
            return false;
        };
        tokio::time::sleep(NEVER).await;
        self.relays
            .get(relay_name)
            .unwrap_or_else(|| panic!("nmp-bdd: unknown relay {relay_name:?}"))
            .wire_record()
            .event_ids
            .len()
            == first.event_attempts
    }
}
