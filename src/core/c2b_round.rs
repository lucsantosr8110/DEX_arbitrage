//! Typed evidence emitted for one physical C2B route at one pinned anchor.

use crate::core::{
    c2b_orchestrator::OrchestratorEvidence, canonical_adapters::PinnedQuoteRecord,
    executable_route_materializer::ExecutableRoutePlan, execution_profile::ExecutionProfile,
    fresh_economics::RouteSimulationResult, phase2d_anchor::AnchorBlock,
};
use ethers::types::{H256, U256};

#[derive(Debug, Clone)]
pub struct RoundEvidence {
    pub structural_cycle_key: String,
    pub anchor: AnchorBlock,
    pub context_hash: H256,
    pub route_plan: ExecutableRoutePlan,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
    pub economics: Option<RouteSimulationResult>,
    pub gross_pnl_atomic: Option<i128>,
    pub gas_used_total: u64,
    pub orchestrator_evidence: Option<OrchestratorEvidence>,
    pub rejected_registry_hit: bool,
    /// Real, sequentially chained per-leg quotes (Phase B of discovery).
    /// Always populated regardless of `execution_profile` — the
    /// `main-pending-dry-run` profile never runs a fork audit, so this is
    /// the only source of real executable amounts downstream consumers
    /// (`ExecutableOpportunity`, pending `eth_call` simulation) can use.
    pub leg_quotes: Vec<PinnedQuoteRecord>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        executable_route_materializer::ForkSetupPlan, fresh_economics::PinnedStateSnapshot,
    };
    use ethers::types::Address;

    fn route_plan() -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(),
            anchor_block: 100,
            start_token: Address::zero(),
            legs: vec![],
            snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan {
                anchor_block: 100,
                caller: Address::zero(),
                tokens: vec![],
                funding: vec![],
                approvals: vec![],
                targets: vec![],
                balance_checks: vec![],
            },
        }
    }

    #[test]
    fn round_evidence_carries_structural_key_and_anchor() {
        let evidence = RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: AnchorBlock {
                number: 100,
                hash: H256::repeat_byte(1),
                selected_from_head: 102,
                confirmation_lag: 2,
            },
            context_hash: H256::zero(),
            route_plan: route_plan(),
            amount_in: U256::one(),
            execution_profile: ExecutionProfile {
                chain_id: 137,
                profile_label: "base".into(),
            },
            economics: None,
            gross_pnl_atomic: None,
            gas_used_total: 0,
            orchestrator_evidence: None,
            rejected_registry_hit: false,
            leg_quotes: vec![],
        };

        assert_eq!(evidence.structural_cycle_key, "k");
        assert_eq!(evidence.anchor.number, 100);
    }
}
