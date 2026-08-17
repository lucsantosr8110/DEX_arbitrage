//! Canonical opportunity assembled from a stable three-anchor evidence window.

use crate::core::{
    c2b_round::RoundEvidence, executable_route_materializer::ExecutableRoutePlan,
    execution_profile::ExecutionProfile,
};
use ethers::{
    types::{Address, H256, U256},
    utils::keccak256,
};

#[derive(Debug, Clone)]
pub struct LegQuote {
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
}

#[derive(Debug, Clone)]
pub struct ExecutionEvidence {
    pub eth_call_pass: bool,
    pub preflight_pass: bool,
    pub trace_validated: bool,
    pub balance_delta: U256,
}

#[derive(Debug, Clone)]
pub struct StabilityRecord {
    pub anchor_blocks: [u64; 3],
    pub anchor_hashes: [H256; 3],
}

#[derive(Debug, Clone)]
pub struct ExecutableOpportunity {
    pub opportunity_id: H256,
    pub structural_cycle_key: String,
    pub route_plan: ExecutableRoutePlan,
    pub anchor_block: u64,
    pub anchor_block_hash: H256,
    pub context_hash: H256,
    pub evidence_hash: H256,
    pub amount_in: U256,
    pub expected_amount_out: U256,
    pub gross_pnl: i128,
    pub gas_estimate: U256,
    pub net_pnl: i128,
    pub net_pnl_usd: Option<f64>,
    pub leg_quotes: Vec<LegQuote>,
    pub evidence: ExecutionEvidence,
    pub stability: StabilityRecord,
    pub execution_profile: ExecutionProfile,
    pub rejected_registry_hit: bool,
}

/// Creates a deterministic opportunity from a window already accepted by the
/// stability gate. The newest evidence is authoritative for executable data.
///
/// Fork receipt/trace evidence (`orchestrator_evidence`) is only ever
/// produced by the diagnostic binary's `fork-trace-audit` profile; the live
/// `main-pending-dry-run` profile never runs a fork and always carries
/// `orchestrator_evidence: None`. Requiring it unconditionally here would
/// make every main-bot opportunity unbuildable, so its absence is only
/// treated as "no fork evidence" (all-false/zero), never as a build failure.
pub fn from_stable_rounds(rounds: [RoundEvidence; 3]) -> Option<ExecutableOpportunity> {
    let [first, second, latest] = rounds;
    let economics = latest.economics.as_ref()?;
    let anchor_blocks = [
        first.anchor.number,
        second.anchor.number,
        latest.anchor.number,
    ];
    let anchor_hashes = [first.anchor.hash, second.anchor.hash, latest.anchor.hash];

    // Real chained-quote amounts come from Phase B of `discover_at`, keyed
    // by (pool, token_in) — never a fabricated or zeroed placeholder.
    let leg_quotes = latest
        .route_plan
        .legs
        .iter()
        .map(|leg| {
            let quote = latest
                .leg_quotes
                .iter()
                .find(|q| q.pool == leg.pool && q.token_in == leg.token_in);
            LegQuote {
                pool: leg.pool,
                token_in: leg.token_in,
                token_out: leg.token_out,
                amount_in: quote.map(|q| q.amount_in).unwrap_or_default(),
                amount_out: quote.map(|q| q.amount_out).unwrap_or_default(),
            }
        })
        .collect();

    let (eth_call_pass, preflight_pass, trace_validated, balance_delta) =
        match &latest.orchestrator_evidence {
            Some(orchestrator) => (
                orchestrator.readonly_pass,
                orchestrator.preflight_pass,
                orchestrator.trace_validated,
                orchestrator.balance_delta,
            ),
            None => (false, false, false, U256::zero()),
        };
    let evidence_hash = H256::from(keccak256(format!(
        "{}:{anchor_blocks:?}:{anchor_hashes:?}:{}:{eth_call_pass}:{preflight_pass}:{trace_validated}",
        latest.structural_cycle_key, economics.net_pnl_atomic,
    )));
    let opportunity_id = H256::from(keccak256(format!(
        "{}:{:?}:{evidence_hash:?}",
        latest.structural_cycle_key, latest.amount_in,
    )));

    Some(ExecutableOpportunity {
        opportunity_id,
        structural_cycle_key: latest.structural_cycle_key.clone(),
        route_plan: latest.route_plan.clone(),
        anchor_block: latest.anchor.number,
        anchor_block_hash: latest.anchor.hash,
        context_hash: latest.context_hash,
        evidence_hash,
        amount_in: latest.amount_in,
        expected_amount_out: economics.final_amount_atomic,
        gross_pnl: economics.gross_pnl_atomic,
        gas_estimate: economics.gas_cost_atomic,
        net_pnl: economics.net_pnl_atomic,
        net_pnl_usd: None,
        leg_quotes,
        evidence: ExecutionEvidence {
            eth_call_pass,
            preflight_pass,
            trace_validated,
            balance_delta,
        },
        stability: StabilityRecord {
            anchor_blocks,
            anchor_hashes,
        },
        execution_profile: latest.execution_profile.clone(),
        rejected_registry_hit: latest.rejected_registry_hit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        c2b_orchestrator::OrchestratorEvidence,
        canonical_adapters::PinnedQuoteRecord,
        executable_call::Venue,
        executable_route_materializer::{ExecutableLegPlan, ForkSetupPlan},
        execution_profile::{FORK_TRACE_AUDIT_PROFILE, MAIN_PENDING_DRY_RUN_PROFILE},
        fresh_economics::{PinnedStateSnapshot, RouteSimulationResult},
        phase2d_anchor::AnchorBlock,
    };

    fn a(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn leg() -> ExecutableLegPlan {
        ExecutableLegPlan {
            venue: Venue::QuickSwap,
            token_in: a(1),
            token_out: a(2),
            pool: a(30),
            router: a(40),
            fee: None,
            curve_method: None,
            token_in_index: None,
            token_out_index: None,
            spender: a(40),
        }
    }

    fn route_plan() -> ExecutableRoutePlan {
        ExecutableRoutePlan {
            structural_cycle_key: "k".into(),
            anchor_block: 100,
            start_token: a(1),
            legs: vec![leg()],
            snapshot: PinnedStateSnapshot::default(),
            fork_setup: ForkSetupPlan {
                anchor_block: 100,
                caller: a(99),
                tokens: vec![],
                funding: vec![],
                approvals: vec![],
                targets: vec![],
                balance_checks: vec![],
            },
        }
    }

    fn quote_record() -> PinnedQuoteRecord {
        PinnedQuoteRecord {
            quote_id: H256::repeat_byte(7),
            anchor_block: 100,
            anchor_hash: H256::repeat_byte(1),
            venue: Venue::QuickSwap,
            pool: a(30),
            token_in: a(1),
            token_out: a(2),
            amount_in: U256::from(1_000u64),
            amount_out: U256::from(1_100u64),
            pool_state_id: H256::repeat_byte(2),
            execution_metadata_id: H256::repeat_byte(3),
            adapter_version: "v1".into(),
            provenance_hash: H256::repeat_byte(4),
        }
    }

    fn round(number: u64, hash_byte: u8, profile_label: &str) -> RoundEvidence {
        RoundEvidence {
            structural_cycle_key: "k".into(),
            anchor: AnchorBlock {
                number,
                hash: H256::repeat_byte(hash_byte),
                selected_from_head: number + 2,
                confirmation_lag: 2,
            },
            context_hash: H256::repeat_byte(9),
            route_plan: route_plan(),
            amount_in: U256::from(1_000u64),
            execution_profile: ExecutionProfile {
                chain_id: 137,
                profile_label: profile_label.into(),
            },
            economics: Some(RouteSimulationResult {
                final_amount_atomic: U256::from(1_100u64),
                gross_pnl_atomic: 100,
                gas_cost_atomic: U256::one(),
                flashloan_cost_atomic: U256::zero(),
                net_pnl_atomic: 99,
                pool_reuse_detected: false,
                all_models_supported: true,
            }),
            gross_pnl_atomic: Some(100),
            gas_used_total: 0,
            orchestrator_evidence: None,
            rejected_registry_hit: false,
            leg_quotes: vec![quote_record()],
        }
    }

    #[test]
    fn main_pending_dry_run_builds_opportunity_without_orchestrator_evidence() {
        let rounds = [
            round(100, 1, MAIN_PENDING_DRY_RUN_PROFILE),
            round(103, 2, MAIN_PENDING_DRY_RUN_PROFILE),
            round(106, 3, MAIN_PENDING_DRY_RUN_PROFILE),
        ];
        let opportunity = from_stable_rounds(rounds).expect("opportunity must build");
        assert_eq!(opportunity.net_pnl, 99);
        assert!(!opportunity.evidence.eth_call_pass);
        assert!(!opportunity.evidence.preflight_pass);
        assert!(!opportunity.evidence.trace_validated);
    }

    #[test]
    fn main_pending_dry_run_carries_real_chained_quote_amounts() {
        let rounds = [
            round(100, 1, MAIN_PENDING_DRY_RUN_PROFILE),
            round(103, 2, MAIN_PENDING_DRY_RUN_PROFILE),
            round(106, 3, MAIN_PENDING_DRY_RUN_PROFILE),
        ];
        let opportunity = from_stable_rounds(rounds).expect("opportunity must build");
        assert_eq!(opportunity.leg_quotes.len(), 1);
        assert_eq!(opportunity.leg_quotes[0].amount_in, U256::from(1_000u64));
        assert_eq!(opportunity.leg_quotes[0].amount_out, U256::from(1_100u64));
    }

    #[test]
    fn fork_trace_audit_round_still_carries_real_orchestrator_evidence() {
        let mut latest = round(106, 3, FORK_TRACE_AUDIT_PROFILE);
        latest.orchestrator_evidence = Some(OrchestratorEvidence {
            structural_cycle_key: "k".into(),
            economic_positive: true,
            builder_called: true,
            readonly_pass: true,
            preflight_pass: true,
            balance_delta: U256::from(42u64),
            output_propagated: true,
            trace_validated: true,
            rejected_registry_hit: false,
            placeholder_evidence: false,
        });
        let rounds = [
            round(100, 1, FORK_TRACE_AUDIT_PROFILE),
            round(103, 2, FORK_TRACE_AUDIT_PROFILE),
            latest,
        ];
        let opportunity = from_stable_rounds(rounds).expect("opportunity must build");
        assert!(opportunity.evidence.eth_call_pass);
        assert!(opportunity.evidence.preflight_pass);
        assert!(opportunity.evidence.trace_validated);
        assert_eq!(opportunity.evidence.balance_delta, U256::from(42u64));
    }

    #[test]
    fn missing_economics_yields_no_opportunity() {
        let mut latest = round(106, 3, MAIN_PENDING_DRY_RUN_PROFILE);
        latest.economics = None;
        let rounds = [
            round(100, 1, MAIN_PENDING_DRY_RUN_PROFILE),
            round(103, 2, MAIN_PENDING_DRY_RUN_PROFILE),
            latest,
        ];
        assert!(from_stable_rounds(rounds).is_none());
    }
}
