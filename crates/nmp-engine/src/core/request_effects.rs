//! Wire effects that carry exact local request-attempt identities (#849).

use std::{collections::BTreeMap, ops::Deref};

use nmp_grammar::{ConcreteFilter, DescriptorHash, RelaySessionKey};
use nmp_router::{SubId, WireDelta, WireOp, WireReq};

use super::{CoreState, RequestAttemptId};

/// A router delta plus the exact attempt identity of every REQ it carries.
#[derive(Debug)]
pub struct AttemptedWireDelta {
    delta: WireDelta,
    attempts: BTreeMap<(RelaySessionKey, SubId, DescriptorHash), RequestAttemptId>,
}

impl AttemptedWireDelta {
    pub(super) fn new(
        delta: WireDelta,
        attempts: BTreeMap<(RelaySessionKey, SubId, DescriptorHash), RequestAttemptId>,
    ) -> Self {
        Self { delta, attempts }
    }

    #[doc(hidden)]
    pub fn attempt_id(
        &self,
        session: &RelaySessionKey,
        sub_id: &SubId,
        filter: &ConcreteFilter,
    ) -> RequestAttemptId {
        self.attempts[&(session.clone(), sub_id.clone(), filter.hash())]
    }
}

impl Deref for AttemptedWireDelta {
    type Target = WireDelta;

    fn deref(&self) -> &Self::Target {
        &self.delta
    }
}

/// A reconnect batch whose request attempt ids are position-aligned by type.
#[derive(Debug)]
pub struct AttemptedReplay {
    requests: Vec<WireReq>,
    attempts: Vec<RequestAttemptId>,
}

impl AttemptedReplay {
    pub(super) fn new(requests: Vec<WireReq>, attempts: Vec<RequestAttemptId>) -> Self {
        assert_eq!(requests.len(), attempts.len());
        Self { requests, attempts }
    }

    #[doc(hidden)]
    pub fn attempts(&self) -> &[RequestAttemptId] {
        &self.attempts
    }
}

impl Deref for AttemptedReplay {
    type Target = [WireReq];

    fn deref(&self) -> &Self::Target {
        &self.requests
    }
}

impl PartialEq<Vec<WireReq>> for AttemptedReplay {
    fn eq(&self, other: &Vec<WireReq>) -> bool {
        self.requests == *other
    }
}

impl CoreState {
    pub(in crate::core) fn attempted_wire_delta(&self, delta: WireDelta) -> AttemptedWireDelta {
        let mut attempts = BTreeMap::new();
        for (session, ops) in &delta.ops {
            for op in ops {
                let WireOp::Req(sub_id, filter) = op else {
                    continue;
                };
                let attempt = self.pending_request_evidence[&(session.clone(), sub_id.clone())]
                    .iter()
                    .rev()
                    .find(|request| request.filter.hash() == filter.hash())
                    .expect("every emitted REQ owns an exact pending attempt")
                    .attempt_id;
                attempts.insert((session.clone(), sub_id.clone(), filter.hash()), attempt);
            }
        }
        AttemptedWireDelta::new(delta, attempts)
    }

    pub(in crate::core) fn attempted_replay(
        &self,
        session: &RelaySessionKey,
        requests: Vec<WireReq>,
    ) -> AttemptedReplay {
        let attempts = requests
            .iter()
            .map(|request| {
                self.pending_request_evidence[&(session.clone(), request.sub_id.clone())]
                    .iter()
                    .rev()
                    .find(|pending| pending.filter.hash() == request.filter.hash())
                    .expect("every replayed REQ owns an exact pending attempt")
                    .attempt_id
            })
            .collect();
        AttemptedReplay::new(requests, attempts)
    }
}
