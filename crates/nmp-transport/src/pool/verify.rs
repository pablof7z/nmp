//! Ingest-time signature verification gate (M3 plan §3.2 hardening) --
//! network-boundary, kind-blind: this module has no notion of "kind" at
//! all, only "is this wire frame an `EVENT`, and if so is its signature
//! genuine". Every `EVENT` frame off the wire is schnorr-verified via
//! rust-nostr's [`Event::verify`] (id + signature) exactly once per
//! distinct event id -- never hand-rolled crypto. A redelivery of an
//! already-verified id (the same event relayed by a second/third relay,
//! the common case for any outbox-routed author) is accepted by a cheap
//! signature STRING compare against the previously-verified value, never a
//! second schnorr operation.
//!
//! A frame that fails verification on first sight, or whose signature
//! mismatches a previously-verified id, is dropped HERE: it never becomes
//! a [`super::PoolEvent::Frame`], so it never reaches the engine, the
//! store, or any routing decision. This is what makes bug-ledger #5's own
//! mechanism text ("ids/signatures never re-derived post-verification")
//! honest -- the verification step it presupposes now actually exists, at
//! the one seam every inbound byte already passes through.
//!
//! Not this module's job: anything kind-aware (routing, coverage, demand).
//! Not this module's job either: deciding what happens to a relay that
//! misbehaves -- it only counts the fact via [`RelayHealth`] and lets the
//! caller (or a future policy layer) decide.

use std::collections::HashMap;

use nostr::secp256k1::schnorr::Signature;
use nostr::{Event, EventId, JsonUtil};

use crate::health::RelayHealth;

use super::RelayFrame;

/// Outcome of the gate for one inbound [`RelayFrame`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum GateVerdict {
    /// Not an `EVENT` frame (or an `EVENT`-shaped frame whose payload did
    /// not even parse as a full [`Event`]) -- passed through unchanged,
    /// exactly this gate's predecessor behavior. A payload that doesn't
    /// parse as a full event is still safely handled downstream by the
    /// engine's own `RelayMessage::from_json` (malformed frame -> dropped
    /// there, same as always) -- this gate only has an opinion on frames it
    /// can fully parse.
    PassThrough,
    /// A verified (fresh or redelivered-and-matching) `EVENT` frame.
    Accept,
    /// A forged/tampered `EVENT` frame: bad schnorr signature on first
    /// sight, or a signature that does not match the previously-verified
    /// value already on file for the same event id. Never forwarded.
    Reject,
}

/// Verify a translator-sized burst concurrently, then update the shared
/// id/signature cache in input order. Parsing stays per frame; expensive
/// schnorr checks for distinct first-seen ids run across scoped native
/// threads. Redeliveries already in `verified` remain byte comparisons.
pub(super) fn gate_batch(
    verified: &mut HashMap<EventId, Signature>,
    frames: &[&RelayFrame],
) -> Vec<GateVerdict> {
    let parsed: Vec<Option<Event>> = frames
        .iter()
        .map(|frame| {
            let RelayFrame::Text(text) = frame else {
                return None;
            };
            let event_json = sniff_event_payload(text)?;
            Event::from_json(event_json).ok()
        })
        .collect();
    let mut verdicts = vec![GateVerdict::PassThrough; frames.len()];
    let mut unknown = Vec::new();
    for (index, event) in parsed.iter().enumerate() {
        let Some(event) = event else { continue };
        if let Some(known_sig) = verified.get(&event.id) {
            verdicts[index] = if *known_sig == event.sig {
                GateVerdict::Accept
            } else {
                GateVerdict::Reject
            };
        } else {
            unknown.push((index, event));
        }
    }

    let checks = verify_unknown(&unknown);
    for ((index, event), valid) in unknown.into_iter().zip(checks) {
        if !valid {
            verdicts[index] = GateVerdict::Reject;
            continue;
        }
        verdicts[index] = match verified.get(&event.id) {
            Some(known_sig) if *known_sig != event.sig => GateVerdict::Reject,
            Some(_) => GateVerdict::Accept,
            None => {
                verified.insert(event.id, event.sig);
                GateVerdict::Accept
            }
        };
    }
    verdicts
}

fn verify_unknown(events: &[(usize, &Event)]) -> Vec<bool> {
    #[cfg(not(target_arch = "wasm32"))]
    if events.len() > 1 {
        let workers = std::thread::available_parallelism()
            .map_or(1, usize::from)
            .min(events.len());
        if workers > 1 {
            let chunk_len = events.len().div_ceil(workers);
            return std::thread::scope(|scope| {
                let handles: Vec<_> = events
                    .chunks(chunk_len)
                    .map(|chunk| {
                        scope.spawn(move || {
                            chunk
                                .iter()
                                .map(|(_index, event)| event.verify().is_ok())
                                .collect::<Vec<_>>()
                        })
                    })
                    .collect();
                handles
                    .into_iter()
                    .flat_map(|handle| handle.join().expect("verification worker panicked"))
                    .collect()
            });
        }
    }
    events
        .iter()
        .map(|(_index, event)| event.verify().is_ok())
        .collect()
}

/// Peek `frame`; verify at most once per distinct event id.
///
/// `verified` is the pool-global cache of event id -> the signature that
/// passed [`Event::verify`] the first time that id was seen, from ANY
/// relay -- shared across every slot (via [`super::inner::PoolInner`], the
/// single owner of every slot) so a redelivery from a second relay never
/// re-runs the schnorr check, only a byte-for-byte signature compare.
pub(super) fn gate(verified: &mut HashMap<EventId, Signature>, frame: &RelayFrame) -> GateVerdict {
    let RelayFrame::Text(text) = frame else {
        return GateVerdict::PassThrough;
    };
    let Some(event_json) = sniff_event_payload(text) else {
        return GateVerdict::PassThrough;
    };
    let Ok(event) = Event::from_json(event_json) else {
        return GateVerdict::PassThrough;
    };

    if let Some(known_sig) = verified.get(&event.id) {
        return if *known_sig == event.sig {
            GateVerdict::Accept
        } else {
            GateVerdict::Reject
        };
    }

    if event.verify().is_ok() {
        verified.insert(event.id, event.sig);
        GateVerdict::Accept
    } else {
        GateVerdict::Reject
    }
}

/// Cheap peek: `["EVENT", <sub_id>, {...}]` -> the embedded event object's
/// raw JSON text, or `None` for anything else (not an `EVENT` frame, or a
/// malformed array shape). Mirrors `pool::frame::classify_text`'s
/// fast-path-substring-before-parsing style, for the same reason: avoid
/// paying a `serde_json` parse for the (overwhelmingly common) non-EVENT
/// frames (`EOSE`/`OK`/`NOTICE`/`CLOSED`/`NEG-*`).
fn sniff_event_payload(text: &str) -> Option<String> {
    if !text.contains("\"EVENT\"") {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(text).ok()?;
    let arr = parsed.as_array()?;
    if arr.len() < 3 || arr[0].as_str() != Some("EVENT") {
        return None;
    }
    Some(arr[2].to_string())
}

/// Bump the misbehavior counter on `health` for a rejected frame -- the
/// relay-health signal the app can observe via `Pool::health` (ledger's own
/// "surface it, never just narrate it" rule). Kept as its own tiny function
/// so the call site in `apply_worker_event` reads as one clear step.
pub(super) fn record_misbehavior(health: &mut RelayHealth) {
    health.invalid_signature_count += 1;
}

#[cfg(test)]
mod tests {
    use super::*;
    use nostr::{EventBuilder, Keys, Kind};

    fn signed_event_frame(keys: &Keys, content: &str) -> (RelayFrame, Event) {
        let event = EventBuilder::new(Kind::TextNote, content)
            .sign_with_keys(keys)
            .expect("test fixture must sign cleanly");
        let frame = RelayFrame::Text(
            nostr::RelayMessage::event(nostr::SubscriptionId::new("s"), event.clone()).as_json(),
        );
        (frame, event)
    }

    #[test]
    fn fresh_valid_event_is_accepted_and_cached() {
        let keys = Keys::generate();
        let (frame, event) = signed_event_frame(&keys, "hello");
        let mut verified = HashMap::new();

        assert_eq!(gate(&mut verified, &frame), GateVerdict::Accept);
        assert_eq!(verified.get(&event.id), Some(&event.sig));
    }

    #[test]
    fn tampered_event_is_rejected_and_not_cached() {
        let keys = Keys::generate();
        let (_, mut event) = signed_event_frame(&keys, "genuine");
        event.content = "forged".to_string();
        let forged_frame = RelayFrame::Text(
            nostr::RelayMessage::event(nostr::SubscriptionId::new("s"), event.clone()).as_json(),
        );
        let mut verified = HashMap::new();

        assert_eq!(gate(&mut verified, &forged_frame), GateVerdict::Reject);
        assert!(
            verified.is_empty(),
            "a rejected event must never be cached as verified"
        );
    }

    #[test]
    fn redelivery_of_a_verified_id_is_accepted_without_a_second_schnorr_check() {
        let keys = Keys::generate();
        let (frame, _event) = signed_event_frame(&keys, "same event, two relays");
        let mut verified = HashMap::new();
        assert_eq!(gate(&mut verified, &frame), GateVerdict::Accept);

        // Simulate a second relay delivering the exact same signed bytes:
        // the identical frame, fed through the gate again. The cache-hit
        // path (a signature STRING compare, not a schnorr op) is the only
        // way this can succeed a second time without re-parsing sec256k1
        // internals differently -- `redelivery_with_a_mismatched_signature_
        // for_a_known_id_is_rejected` below is what actually falsifies that
        // this is a compare and not just "always accept a known id".
        assert_eq!(gate(&mut verified, &frame), GateVerdict::Accept);
        assert_eq!(verified.len(), 1, "no new cache entry for a redelivery");
    }

    #[test]
    fn batch_gate_accepts_and_caches_distinct_valid_events_in_order() {
        let keys = Keys::generate();
        let frames: Vec<_> = (0..16)
            .map(|index| signed_event_frame(&keys, &format!("batch-{index}")).0)
            .collect();
        let refs: Vec<_> = frames.iter().collect();
        let mut verified = HashMap::new();

        let verdicts = gate_batch(&mut verified, &refs);

        assert_eq!(verdicts, vec![GateVerdict::Accept; 16]);
        assert_eq!(verified.len(), 16);
    }

    #[test]
    fn redelivery_with_a_mismatched_signature_for_a_known_id_is_rejected() {
        // Construct two DIFFERENT signed events that happen to share an id
        // by directly forging a second `Event` value with event A's id but
        // event B's signature -- modeling a relay that redelivers a known
        // id with a corrupted/substituted signature.
        let keys = Keys::generate();
        let (frame_a, event_a) = signed_event_frame(&keys, "event a");
        let (_, event_b) = signed_event_frame(&keys, "event b");

        let mut verified = HashMap::new();
        assert_eq!(gate(&mut verified, &frame_a), GateVerdict::Accept);

        let mut mismatched = event_a.clone();
        mismatched.sig = event_b.sig;
        let mismatched_frame = RelayFrame::Text(
            nostr::RelayMessage::event(nostr::SubscriptionId::new("s"), mismatched).as_json(),
        );

        assert_eq!(gate(&mut verified, &mismatched_frame), GateVerdict::Reject);
    }

    #[test]
    fn non_event_frame_passes_through_untouched() {
        let frame =
            RelayFrame::Text(nostr::RelayMessage::eose(nostr::SubscriptionId::new("s")).as_json());
        let mut verified = HashMap::new();
        assert_eq!(gate(&mut verified, &frame), GateVerdict::PassThrough);
    }

    #[test]
    fn auth_frame_passes_through_untouched() {
        let frame = RelayFrame::Auth("challenge".to_string());
        let mut verified = HashMap::new();
        assert_eq!(gate(&mut verified, &frame), GateVerdict::PassThrough);
    }
}
