//! Rolling, fail-closed three-anchor stability gate for canonical C2B routes.

use crate::core::{c2b_round::RoundEvidence, execution_profile::ExecutionProfile};
use ethers::types::{Address, U256};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct StabilityKey {
    pub structural_cycle_key: String,
    pub start_token: Address,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
}

impl StabilityKey {
    fn from_evidence(evidence: &RoundEvidence) -> Self {
        Self {
            structural_cycle_key: evidence.structural_cycle_key.clone(),
            start_token: evidence.route_plan.start_token,
            amount_in: evidence.amount_in,
            execution_profile: evidence.execution_profile.clone(),
        }
    }
}

/// Returns true only when all three independent anchors carry complete,
/// profitable and verified execution evidence.
pub fn is_stable(entries: &[RoundEvidence]) -> bool {
    if entries.len() != 3 {
        return false;
    }

    let anchors: BTreeSet<u64> = entries.iter().map(|entry| entry.anchor.number).collect();
    let hashes: BTreeSet<_> = entries.iter().map(|entry| entry.anchor.hash).collect();
    if anchors.len() != 3
        || hashes.len() != 3
        || entries.iter().any(|entry| entry.rejected_registry_hit)
    {
        return false;
    }

    entries.iter().all(|entry| {
        let positive_economics = entry
            .economics
            .as_ref()
            .is_some_and(|economics| economics.net_pnl_atomic > 0)
            && entry.gross_pnl_atomic.is_some_and(|pnl| pnl > 0);
        if !positive_economics {
            return false;
        }
        // Only the fork-trace-audit profile ever produces real Anvil
        // receipt/trace evidence. `main-pending-dry-run` (the live bot's
        // canonical-primary loop) never runs a fork and must not be blocked
        // waiting on evidence it structurally cannot produce.
        if !entry.execution_profile.requires_fork_evidence() {
            return true;
        }
        entry
            .orchestrator_evidence
            .as_ref()
            .is_some_and(|evidence| {
                evidence.economic_positive
                    && evidence.builder_called
                    && evidence.readonly_pass
                    && evidence.preflight_pass
                    && evidence.output_propagated
                    && evidence.trace_validated
                    && !evidence.rejected_registry_hit
                    && !evidence.placeholder_evidence
            })
    })
}

#[derive(Debug, Default)]
pub struct StableOpportunityAggregator {
    windows: BTreeMap<StabilityKey, VecDeque<RoundEvidence>>,
}

impl StableOpportunityAggregator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Retains at most the three newest entries for an identity key. The
    /// scheduler is responsible for submitting anchors in ascending order.
    pub fn push(&mut self, evidence: RoundEvidence) -> Option<[RoundEvidence; 3]> {
        let key = StabilityKey::from_evidence(&evidence);
        let window = self.windows.entry(key).or_default();
        window.push_back(evidence);
        while window.len() > 3 {
            window.pop_front();
        }

        if window.len() != 3 {
            return None;
        }
        let entries: Vec<_> = window.iter().cloned().collect();
        is_stable(&entries).then(|| {
            let mut entries = entries.into_iter();
            [
                entries.next().expect("three entries"),
                entries.next().expect("three entries"),
                entries.next().expect("three entries"),
            ]
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        c2b_orchestrator::OrchestratorEvidence,
        executable_route_materializer::{ExecutableRoutePlan, ForkSetupPlan},
        fresh_economics::{PinnedStateSnapshot, RouteSimulationResult},
        phase2d_anchor::AnchorBlock,
    };
    use ethers::types::H256;

    fn evidence(number: u64, hash_byte: u8, pass: bool) -> RoundEvidence {
        RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: AnchorBlock {
                number,
                hash: H256::repeat_byte(hash_byte),
                selected_from_head: number + 2,
                confirmation_lag: 2,
            },
            context_hash: H256::repeat_byte(9),
            route_plan: ExecutableRoutePlan {
                structural_cycle_key: "k".into(),
                anchor_block: number,
                start_token: Address::zero(),
                legs: vec![],
                snapshot: PinnedStateSnapshot::default(),
                fork_setup: ForkSetupPlan {
                    anchor_block: number,
                    caller: Address::zero(),
                    tokens: vec![],
                    funding: vec![],
                    approvals: vec![],
                    targets: vec![],
                    balance_checks: vec![],
                },
            },
            amount_in: U256::from(1_000u64),
            execution_profile: ExecutionProfile {
                chain_id: 137,
                profile_label: crate::core::execution_profile::FORK_TRACE_AUDIT_PROFILE.into(),
            },
            economics: Some(RouteSimulationResult {
                final_amount_atomic: U256::from(1_100u64),
                gross_pnl_atomic: 100,
                gas_cost_atomic: U256::one(),
                net_pnl_atomic: 99,
                pool_reuse_detected: false,
                all_models_supported: true,
            }),
            gross_pnl_atomic: Some(100),
            gas_used_total: 21_000,
            orchestrator_evidence: Some(OrchestratorEvidence {
                structural_cycle_key: "k".into(),
                economic_positive: true,
                builder_called: true,
                readonly_pass: pass,
                preflight_pass: pass,
                balance_delta: U256::from(100u64),
                output_propagated: pass,
                trace_validated: pass,
                rejected_registry_hit: false,
                placeholder_evidence: false,
            }),
            rejected_registry_hit: false,
            leg_quotes: vec![],
        }
    }

    #[test]
    fn three_distinct_anchors_all_passing_is_stable() {
        assert!(is_stable(&[
            evidence(100, 1, true),
            evidence(103, 2, true),
            evidence(106, 3, true)
        ]));
    }
    #[test]
    fn fewer_than_three_entries_is_not_stable() {
        assert!(!is_stable(&[
            evidence(100, 1, true),
            evidence(103, 2, true)
        ]));
    }
    #[test]
    fn repeated_anchor_is_not_stable() {
        assert!(!is_stable(&[
            evidence(100, 1, true),
            evidence(100, 1, true),
            evidence(103, 2, true)
        ]));
    }
    #[test]
    fn failing_round_is_not_stable() {
        assert!(!is_stable(&[
            evidence(100, 1, true),
            evidence(103, 2, false),
            evidence(106, 3, true)
        ]));
    }
    #[test]
    fn rejected_route_is_not_stable() {
        let mut rejected = evidence(103, 2, true);
        rejected.rejected_registry_hit = true;
        assert!(!is_stable(&[
            evidence(100, 1, true),
            rejected,
            evidence(106, 3, true)
        ]));
    }
    #[test]
    fn aggregator_emits_after_third_matching_push() {
        let mut aggregator = StableOpportunityAggregator::new();
        assert!(aggregator.push(evidence(100, 1, true)).is_none());
        assert!(aggregator.push(evidence(103, 2, true)).is_none());
        assert!(aggregator.push(evidence(106, 3, true)).is_some());
    }
    #[test]
    fn aggregator_keeps_a_rolling_window() {
        let mut aggregator = StableOpportunityAggregator::new();
        aggregator.push(evidence(100, 1, true));
        aggregator.push(evidence(103, 2, true));
        aggregator.push(evidence(106, 3, true));
        let output = aggregator.push(evidence(109, 4, true)).unwrap();
        assert_eq!(output.map(|entry| entry.anchor.number), [103, 106, 109]);
    }

    fn main_pending_dry_run_evidence(number: u64, hash_byte: u8) -> RoundEvidence {
        let mut entry = evidence(number, hash_byte, true);
        entry.execution_profile = ExecutionProfile {
            chain_id: 137,
            profile_label: crate::core::execution_profile::MAIN_PENDING_DRY_RUN_PROFILE.into(),
        };
        entry.orchestrator_evidence = None;
        entry
    }

    #[test]
    fn main_pending_dry_run_is_stable_without_orchestrator_evidence() {
        assert!(is_stable(&[
            main_pending_dry_run_evidence(100, 1),
            main_pending_dry_run_evidence(103, 2),
            main_pending_dry_run_evidence(106, 3),
        ]));
    }

    #[test]
    fn main_pending_dry_run_still_requires_positive_economics() {
        let mut negative = main_pending_dry_run_evidence(103, 2);
        negative.gross_pnl_atomic = Some(-1);
        assert!(!is_stable(&[
            main_pending_dry_run_evidence(100, 1),
            negative,
            main_pending_dry_run_evidence(106, 3),
        ]));
    }

    #[test]
    fn fork_trace_audit_still_requires_orchestrator_evidence() {
        let mut missing_evidence = evidence(103, 2, true);
        missing_evidence.orchestrator_evidence = None;
        assert!(!is_stable(&[
            evidence(100, 1, true),
            missing_evidence,
            evidence(106, 3, true),
        ]));
    }
}
