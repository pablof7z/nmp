/// Closed bounds shared by native content clients.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct HydrationPolicy {
    pub max_active_references: u32,
    pub max_resolved_references: u32,
    pub max_depth: u8,
}

impl Default for HydrationPolicy {
    fn default() -> Self {
        Self {
            max_active_references: 24,
            max_resolved_references: 96,
            max_depth: 3,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ClaimDecision {
    Acquire,
    Cycle { target_key: String },
    DepthLimit { maximum: u8 },
    ActiveLimit { maximum: u32 },
}

/// Decide whether a claimed reference may acquire. Native clients supply only
/// current closed values; no callback or UI lifecycle crosses into Rust.
#[must_use]
pub fn evaluate_claim(
    target_key: &str,
    path: &[String],
    depth: u8,
    active_references: u32,
    policy: HydrationPolicy,
) -> ClaimDecision {
    if path.iter().any(|ancestor| ancestor == target_key) {
        return ClaimDecision::Cycle {
            target_key: target_key.to_string(),
        };
    }
    if depth >= policy.max_depth {
        return ClaimDecision::DepthLimit {
            maximum: policy.max_depth,
        };
    }
    if active_references >= policy.max_active_references {
        return ClaimDecision::ActiveLimit {
            maximum: policy.max_active_references,
        };
    }
    ClaimDecision::Acquire
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolutionDecision {
    Accept,
    ResolvedLimit { maximum: u32 },
}

#[must_use]
pub fn evaluate_resolution(
    target_already_resolved: bool,
    resolved_references: u32,
    policy: HydrationPolicy,
) -> ResolutionDecision {
    if !target_already_resolved && resolved_references >= policy.max_resolved_references {
        ResolutionDecision::ResolvedLimit {
            maximum: policy.max_resolved_references,
        }
    } else {
        ResolutionDecision::Accept
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_precedes_depth_and_budget() {
        let policy = HydrationPolicy {
            max_active_references: 0,
            max_resolved_references: 0,
            max_depth: 0,
        };
        assert_eq!(
            evaluate_claim("a", &["a".to_string()], 4, 10, policy),
            ClaimDecision::Cycle {
                target_key: "a".to_string()
            }
        );
    }

    #[test]
    fn existing_resolution_does_not_consume_the_total_budget_twice() {
        let policy = HydrationPolicy {
            max_resolved_references: 1,
            ..HydrationPolicy::default()
        };
        assert_eq!(
            evaluate_resolution(true, 1, policy),
            ResolutionDecision::Accept
        );
        assert_eq!(
            evaluate_resolution(false, 1, policy),
            ResolutionDecision::ResolvedLimit { maximum: 1 }
        );
    }
}
