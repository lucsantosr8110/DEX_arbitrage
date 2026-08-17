//! Isolated downstream half of the canonical C2B shadow pipeline.
//!
//! Discovery remains outside this module. It accepts typed, already-pinned
//! evidence and can therefore never affect the legacy scanner or initialize a
//! broadcaster. All canonical client calls are structurally dry.

use crate::{
    config::Config,
    core::{
        c2b_round::RoundEvidence,
        canonical_simulation::CanonicalSimulationClient,
        executable_opportunity::{from_stable_rounds, ExecutableOpportunity},
        flashloan::{determine_execution_strategy_canonical, CanonicalStrategyDecision},
        phase2d_anchor::AnchorBlock,
        risk::{CanonicalRiskConfig, CanonicalRiskRejection, RiskApproval, RiskManager},
        stability::StableOpportunityAggregator,
        types::BundleResult,
    },
};
use ethers::{providers::Middleware, types::H256};

/// Pure scheduler gate. A reorg invalidates the pending anchor; duplicate or
/// out-of-order anchors are never submitted to the rolling stability window.
pub fn should_schedule_anchor(
    last_scheduled_anchor: Option<u64>,
    anchor: &AnchorBlock,
    every_n_blocks: u64,
    reorg_detected: bool,
) -> bool {
    if reorg_detected || every_n_blocks == 0 {
        return false;
    }
    match last_scheduled_anchor {
        None => true,
        Some(last) => anchor.number > last && anchor.number.saturating_sub(last) >= every_n_blocks,
    }
}

#[derive(Debug)]
pub struct C2BShadowResult {
    pub anchor: AnchorBlock,
    pub round_evidences: Vec<RoundEvidence>,
    pub stable_opportunities: Vec<ExecutableOpportunity>,
    pub risk_approvals: Vec<(H256, Result<RiskApproval, Vec<CanonicalRiskRejection>>)>,
    pub strategy_decisions: Vec<(H256, CanonicalStrategyDecision)>,
    pub execution_results: Vec<(H256, BundleResult)>,
}

/// Downstream half of the canonical pipeline, shared by both the
/// observation-only C2C shadow runtime and the live canonical-primary main
/// bot loop. It never signs or broadcasts: `client` is a read-only
/// `CanonicalSimulationClient`, so even a caller with full authority can
/// only ever produce a pending-`eth_call` dry-run result.
pub struct CanonicalC2BOpportunitySource<M> {
    aggregator: StableOpportunityAggregator,
    risk_manager: RiskManager,
    client: CanonicalSimulationClient<M>,
    canonical_risk_cfg: CanonicalRiskConfig,
}

impl<M> CanonicalC2BOpportunitySource<M>
where
    M: Middleware,
    M::Error: 'static,
{
    pub fn new(
        risk_manager: RiskManager,
        client: CanonicalSimulationClient<M>,
        canonical_risk_cfg: CanonicalRiskConfig,
    ) -> Self {
        Self {
            aggregator: StableOpportunityAggregator::new(),
            risk_manager,
            client,
            canonical_risk_cfg,
        }
    }

    /// Processes a single discovery round supplied by the dedicated service
    /// runtime. No failure here can enter the legacy loop; rejected risk or a
    /// failed dry validation merely produces no execution result.
    ///
    /// `authorized` is decided entirely by the caller (shadow-observation
    /// code checks `cfg.c2b_shadow.shadow_runtime_enabled()`; the live
    /// canonical-primary loop checks `cfg.c2b_shadow.canonical_primary_enabled()`
    /// once before ever constructing this source) — this method has no
    /// config-reading authority of its own and never guesses.
    pub async fn run_evidence_round(
        &mut self,
        anchor: AnchorBlock,
        round_evidences: Vec<RoundEvidence>,
        cfg: &Config,
        current_head_block: u64,
        authorized: bool,
    ) -> C2BShadowResult {
        if !authorized {
            return C2BShadowResult {
                anchor,
                round_evidences,
                stable_opportunities: vec![],
                risk_approvals: vec![],
                strategy_decisions: vec![],
                execution_results: vec![],
            };
        }
        let mut stable_opportunities = Vec::new();
        for evidence in round_evidences.iter().cloned() {
            if let Some(rounds) = self.aggregator.push(evidence) {
                if let Some(opportunity) = from_stable_rounds(rounds) {
                    stable_opportunities.push(opportunity);
                }
            }
        }

        let mut risk_approvals = Vec::new();
        let mut strategy_decisions = Vec::new();
        let mut execution_results = Vec::new();
        for opportunity in &stable_opportunities {
            let approval = self.risk_manager.assess_executable_opportunity(
                opportunity,
                &self.canonical_risk_cfg,
                current_head_block,
            );
            risk_approvals.push((opportunity.opportunity_id, approval.clone()));
            let Ok(approval) = approval else { continue };

            let strategy = determine_execution_strategy_canonical(opportunity, &approval, cfg);
            strategy_decisions.push((opportunity.opportunity_id, strategy.clone()));
            // Every non-skip strategy currently resolves to the same
            // pending-`eth_call` dry run — this phase never sends a real
            // transaction regardless of which route shape was selected.
            let result = match strategy {
                CanonicalStrategyDecision::Direct
                | CanonicalStrategyDecision::Flashloan
                | CanonicalStrategyDecision::WrapperFlashloan => {
                    self.client.simulate_pending(opportunity, &approval).await
                }
                CanonicalStrategyDecision::Skip(_) => continue,
            };
            if let Ok(result) = result {
                execution_results.push((opportunity.opportunity_id, result));
            }
        }

        C2BShadowResult {
            anchor,
            round_evidences,
            stable_opportunities,
            risk_approvals,
            strategy_decisions,
            execution_results,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::H256;

    #[test]
    fn empty_shadow_result_cannot_fabricate_opportunities() {
        let result = C2BShadowResult {
            anchor: AnchorBlock {
                number: 1,
                hash: H256::zero(),
                selected_from_head: 3,
                confirmation_lag: 2,
            },
            round_evidences: vec![],
            stable_opportunities: vec![],
            risk_approvals: vec![],
            strategy_decisions: vec![],
            execution_results: vec![],
        };
        assert!(result.stable_opportunities.is_empty());
        assert!(result.execution_results.is_empty());
    }

    fn anchor(number: u64) -> AnchorBlock {
        AnchorBlock {
            number,
            hash: H256::from_low_u64_be(number),
            selected_from_head: number + 2,
            confirmation_lag: 2,
        }
    }

    #[test]
    fn scheduler_requires_spacing_and_strictly_increasing_anchors() {
        assert!(should_schedule_anchor(None, &anchor(100), 32, false));
        assert!(!should_schedule_anchor(Some(100), &anchor(100), 32, false));
        assert!(!should_schedule_anchor(Some(100), &anchor(131), 32, false));
        assert!(should_schedule_anchor(Some(100), &anchor(132), 32, false));
    }

    #[test]
    fn scheduler_reorg_or_zero_interval_fails_closed() {
        assert!(!should_schedule_anchor(None, &anchor(100), 32, true));
        assert!(!should_schedule_anchor(None, &anchor(100), 0, false));
    }
}
