use std::collections::BTreeMap;

use nostr::RelayUrl;

use super::{RelayState, WriteFact, WriteOutcome};

/// The terminal answer for one accepted write.
///
/// `outcome` says why the whole receipt ended. `relays` preserves the last
/// fact NMP recorded for every destination, so disagreement is never reduced
/// to a misleading success/failure boolean.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReceiptResult {
    pub outcome: WriteOutcome,
    pub relays: BTreeMap<RelayUrl, RelayState>,
}

/// Why a receipt could not be reduced to its promised terminal answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptResultError {
    /// Delivery ended before the one whole-write outcome was available.
    ClosedWithoutOutcome,
    /// Durable replay for this accepted receipt is absent or unreadable.
    ReplayUnavailable,
}

impl std::fmt::Display for ReceiptResultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ClosedWithoutOutcome => {
                f.write_str("receipt delivery closed before its terminal outcome")
            }
            Self::ReplayUnavailable => f.write_str("durable receipt replay is unavailable"),
        }
    }
}

impl std::error::Error for ReceiptResultError {}

impl ReceiptResult {
    /// Reduce retained/live facts with NMP's one authoritative receipt rule.
    pub fn from_facts(
        facts: impl IntoIterator<Item = WriteFact>,
    ) -> Result<Self, ReceiptResultError> {
        let mut relays = BTreeMap::new();
        for fact in facts {
            match fact {
                WriteFact::Relay { relay, state, .. } => {
                    relays.insert(relay, state);
                }
                WriteFact::Outcome(outcome) => return Ok(Self { outcome, relays }),
                WriteFact::Signing(_) | WriteFact::Destinations { .. } => {}
            }
        }
        Err(ReceiptResultError::ClosedWithoutOutcome)
    }
}

#[cfg(test)]
mod tests {
    use nostr::{EventId, RelayUrl, Timestamp};

    use super::*;

    #[test]
    fn mixed_relay_result_preserves_each_terminal_truth() {
        let published = RelayUrl::parse("wss://published.example").unwrap();
        let rejected = RelayUrl::parse("wss://rejected.example").unwrap();
        let result = ReceiptResult::from_facts([
            WriteFact::Relay {
                event_id: EventId::from_byte_array([1; 32]),
                relay: published.clone(),
                state: RelayState::Published,
            },
            WriteFact::Relay {
                event_id: EventId::from_byte_array([1; 32]),
                relay: rejected.clone(),
                state: RelayState::Rejected {
                    reason: "blocked".to_string(),
                },
            },
            WriteFact::Outcome(WriteOutcome::Settled),
        ])
        .unwrap();

        assert_eq!(result.outcome, WriteOutcome::Settled);
        assert_eq!(result.relays[&published], RelayState::Published);
        assert_eq!(
            result.relays[&rejected],
            RelayState::Rejected {
                reason: "blocked".to_string()
            }
        );
    }

    #[test]
    fn missing_terminal_outcome_is_typed() {
        let relay = RelayUrl::parse("wss://pending.example").unwrap();
        assert_eq!(
            ReceiptResult::from_facts([WriteFact::Relay {
                event_id: EventId::from_byte_array([1; 32]),
                relay,
                state: RelayState::Sent {
                    attempt: 1,
                    written_at: Timestamp::from(1u64),
                },
            }]),
            Err(ReceiptResultError::ClosedWithoutOutcome)
        );
    }
}
