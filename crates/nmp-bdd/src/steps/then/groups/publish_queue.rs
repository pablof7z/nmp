//! What reached which host, and what reached nothing at all.

use cucumber::then;

use super::*;

// ---- what the host received, and what nothing else did -------------------

#[then(regex = r#"^the published event carries an h tag with value "([^"]+)"$"#)]
async fn published_carries_h(w: &mut NmpWorld, value: String) {
    let event = delivered(w).await;
    assert_eq!(
        values_of(&event, "h"),
        vec![value.clone()],
        "expected exactly one h row naming {value:?}; rows were {:?}",
        rows(&event)
    );
}

#[then(regex = r#"^the published event is kind (\d+)$"#)]
async fn published_is_kind(w: &mut NmpWorld, kind: u16) {
    let event = delivered(w).await;
    assert_eq!(event.kind.as_u16(), kind);
}

/// "Delivered to <host>" is read off the SOCKET -- the host was handed exactly
/// these bytes -- conjuncted with the receipt saying NMP wrote them there.
///
/// Deliberately not the `Acked` beat alone, unlike [`super::writes`]'s
/// note-shaped sibling. What a group scenario asserts is where the event WENT,
/// and the fixture relay's storage engine has opinions of its own that have
/// nothing to do with routing: `nostr-memory` refuses an addressable event
/// (kind 30000-39999) that carries no `d` row, so the kind-blindness outline's
/// custom kind would fail an ack-only reading for a reason no part of NMP
/// caused. The socket witness is both stronger (it names the exact bytes) and
/// about the right thing.
#[then(regex = r#"^(?:the published event|the join request) was delivered to "([^"]+)"$"#)]
pub(super) async fn published_delivered_to(w: &mut NmpWorld, relay: String) {
    let url = w.relay_url(&relay);
    let wrote_it = w.receipt_eventually(|seen| {
        seen.iter().any(|s| {
            matches!(
                s,
                WriteFact::Relay {
                    relay,
                    state: RelayState::Sent { .. } | RelayState::Published,
                    ..
                } if *relay == url
            )
        })
    });
    assert!(
        wrote_it,
        "expected the group write to reach {relay:?}; receipt showed {:?}",
        w.receipt_statuses()
    );
    assert!(
        w.delivered_event_at(&relay).await.is_some(),
        "the receipt says NMP wrote to {relay:?}, but that relay was never handed the event"
    );
}

#[then(regex = r#"^no other relay received the published event$"#)]
async fn no_other_relay_received_it(w: &mut NmpWorld) {
    settled(w).await;
    let host = w.group_host_name(None);
    let id = w
        .published_event_id()
        .expect("nmp-bdd: nothing was published, so 'no OTHER relay' proves nothing");
    let strays: Vec<String> = w
        .relays_holding_published_event(id)
        .into_iter()
        .filter(|name| *name != host)
        .collect();
    assert!(
        strays.is_empty(),
        "only the group's host {host:?} may receive a group write, but {strays:?} did too"
    );
}

#[then(regex = r#"^it was delivered to "([^"]+)" and to no other relay$"#)]
async fn delivered_only_to(w: &mut NmpWorld, relay: String) {
    published_delivered_to(w, relay).await;
    no_other_relay_received_it(w).await;
}

#[then(regex = r#"^relay "([^"]+)" received no event$"#)]
async fn relay_received_no_event(w: &mut NmpWorld, relay: String) {
    settled(w).await;
    let received = w.events_received_by(&relay);
    assert!(
        received.is_empty(),
        "expected {relay:?} to receive no event, but it was handed kinds {:?}",
        received.iter().map(|e| e.kind.as_u16()).collect::<Vec<_>>()
    );
}

#[then(regex = r#"^no relay received the event$"#)]
async fn no_relay_received_the_event(w: &mut NmpWorld) {
    settled(w).await;
    let carriers: Vec<String> = w
        .relay_names()
        .filter(|name| !w.events_received_by(name).is_empty())
        .cloned()
        .collect();
    assert!(
        carriers.is_empty(),
        "a refused publication must reach no relay at all, but {carriers:?} were handed an event"
    );
}

#[then(regex = r#"^relay "([^"]+)" received only the event carrying h "([^"]+)"$"#)]
async fn relay_received_only_h(w: &mut NmpWorld, relay: String, group_id: String) {
    settled(w).await;
    let received = w.events_received_by(&relay);
    assert!(
        !received.is_empty(),
        "NOTHING TO OBSERVE -- {relay:?} was handed no event at all, so 'only the \
         event carrying h {group_id:?}' would hold for an empty relay"
    );
    for event in &received {
        assert_eq!(
            values_of(event, "h"),
            vec![group_id.clone()],
            "{relay:?} may only hold its own group's events, but was handed {:?}",
            rows(event)
        );
    }
}

#[then(regex = r#"^every contacted relay is "([^"]+)"$"#)]
async fn every_contacted_relay_is(w: &mut NmpWorld, relay: String) {
    settled(w).await;
    assert!(
        w.relay_contacted(&relay),
        "NOTHING TO OBSERVE -- the group's own host {relay:?} was never contacted, \
         so 'every contacted relay is it' holds over an empty set"
    );
    let strays: Vec<String> = w
        .relay_names()
        .filter(|name| **name != relay && w.relay_contacted(name))
        .cloned()
        .collect();
    assert!(
        strays.is_empty(),
        "these relays were also contacted: {strays:?}"
    );
}
