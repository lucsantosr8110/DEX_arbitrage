//! Phase 2D — canonical primary wiring: deterministic end-to-end test.
//!
//! Drives the full downstream pipeline —
//! `RoundEvidence` -> `StableOpportunityAggregator` -> `ExecutableOpportunity`
//! -> `RiskManager` -> `determine_execution_strategy_canonical` ->
//! `CanonicalSimulationClient` (pending `eth_call`) — with three synthetic,
//! deterministic `RoundEvidence` fixtures standing in for three real
//! `discover_at` rounds. No network is used: the only RPC surface
//! (`eth_call`) is an `ethers::providers::MockProvider`, so this test never
//! touches a real archive RPC (unlike `tests/canonical_discovery_operational.rs`,
//! which proves `discover_at` itself against a real pinned fork).

use ethers::{
    providers::{MockProvider, Provider},
    types::{Address, Bytes, H256, U256},
};
use flashloan_bot::config::Config;
use flashloan_bot::core::{
    c2b_round::RoundEvidence,
    c2b_shadow_service::CanonicalC2BOpportunitySource,
    canonical_adapters::PinnedQuoteRecord,
    canonical_simulation::CanonicalSimulationClient,
    executable_call::Venue,
    executable_route_materializer::{ExecutableLegPlan, ExecutableRoutePlan, ForkSetupPlan},
    execution_profile::{ExecutionProfile, FORK_TRACE_AUDIT_PROFILE, MAIN_PENDING_DRY_RUN_PROFILE},
    flashloan::CanonicalStrategyDecision,
    fresh_economics::{PinnedStateSnapshot, RouteSimulationResult},
    phase2d_anchor::AnchorBlock,
    risk::{CanonicalRiskConfig, RiskManager},
    types::RiskConfig,
};
use std::sync::Arc;

fn a(n: u64) -> Address {
    Address::from_low_u64_be(n)
}

fn route_plan() -> ExecutableRoutePlan {
    ExecutableRoutePlan {
        structural_cycle_key: "k".into(),
        anchor_block: 100,
        start_token: a(1),
        legs: vec![ExecutableLegPlan {
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
        }],
        snapshot: PinnedStateSnapshot::default(),
        fork_setup: ForkSetupPlan {
            anchor_block: 100,
            caller: a(99),
            tokens: vec![a(1), a(2)],
            funding: vec![],
            approvals: vec![],
            targets: vec![a(40)],
            balance_checks: vec![],
        },
    }
}

fn leg_quote() -> PinnedQuoteRecord {
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

/// Stands in for one `discover_at` round's `RoundEvidence` output.
fn evidence(
    number: u64,
    hash_byte: u8,
    net_pnl_atomic: i128,
    profile_label: &str,
) -> RoundEvidence {
    RoundEvidence {
        structural_cycle_key: "k".into(),
        anchor: AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
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
            net_pnl_atomic,
            pool_reuse_detected: false,
            all_models_supported: true,
        }),
        gross_pnl_atomic: Some(100),
        gas_used_total: 0,
        orchestrator_evidence: None,
        rejected_registry_hit: false,
        leg_quotes: vec![leg_quote()],
    }
}

fn permissive_risk_cfg() -> CanonicalRiskConfig {
    CanonicalRiskConfig {
        // Must be non-zero: `determine_execution_strategy_canonical` skips
        // as `InsufficientRiskApproval` whenever `min_profit_raw` is zero.
        absolute_min_profit_floor_raw: U256::one(),
        retention_bps: 0,
        max_gas_raw: U256::from(1_000_000u64),
        max_slippage_bps: 10_000,
        max_anchor_age_blocks: 1_000_000,
    }
}

fn source_with_mock() -> (
    CanonicalC2BOpportunitySource<Provider<MockProvider>>,
    MockProvider,
) {
    let (provider, mock) = Provider::<MockProvider>::mocked();
    let client = CanonicalSimulationClient::new(Arc::new(provider), a(99));
    let source = CanonicalC2BOpportunitySource::new(
        RiskManager::new(RiskConfig::default()),
        client,
        permissive_risk_cfg(),
    );
    (source, mock)
}

#[tokio::test]
async fn three_stable_rounds_reach_a_dry_run_result() {
    let (mut source, mock) = source_with_mock();
    // Exactly one leg -> exactly one pending `eth_call` on the round the
    // aggregator actually emits a stable opportunity (round 3).
    mock.push::<Bytes, Bytes>(Bytes::default()).unwrap();

    let cfg = Config::default();
    let anchors = [(100u64, 1u8), (103, 2), (106, 3)];
    let mut results = Vec::new();
    for (number, hash_byte) in anchors {
        let round_evidence = vec![evidence(
            number,
            hash_byte,
            99,
            MAIN_PENDING_DRY_RUN_PROFILE,
        )];
        let anchor = AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
        };
        let result = source
            .run_evidence_round(anchor, round_evidence, &cfg, number, true)
            .await;
        results.push(result);
    }

    // CANONICAL_DISCOVERY_CALLS=3 (one per synthetic round)
    assert_eq!(results.len(), 3);
    // Rounds 1-2: window incomplete, nothing downstream runs.
    for result in &results[..2] {
        assert!(result.stable_opportunities.is_empty());
        assert!(result.risk_approvals.is_empty());
        assert!(result.strategy_decisions.is_empty());
        assert!(result.execution_results.is_empty());
    }
    // Round 3: CANONICAL_AGGREGATOR_EMISSIONS=1
    let last = &results[2];
    assert_eq!(
        last.stable_opportunities.len(),
        1,
        "CANONICAL_AGGREGATOR_EMISSIONS=1"
    );
    // CANONICAL_RISK_CALLS=1
    assert_eq!(last.risk_approvals.len(), 1);
    assert!(last.risk_approvals[0].1.is_ok());
    // CANONICAL_STRATEGY_CALLS=1
    assert_eq!(last.strategy_decisions.len(), 1);
    assert!(!matches!(
        last.strategy_decisions[0].1,
        CanonicalStrategyDecision::Skip(_)
    ));
    // CANONICAL_EXECUTOR_CALLS=1 / PENDING_SIMULATION_CALLS=1
    assert_eq!(
        last.execution_results.len(),
        1,
        "PENDING_SIMULATION_CALLS=1"
    );
    let (_, bundle) = &last.execution_results[0];
    assert!(bundle.success);
    assert_eq!(
        bundle.execution_mode.as_deref(),
        Some("canonical_dry_run_completed")
    );
    // SEND_AND_CONFIRM_CALLS=0: never a transaction hash.
    assert!(bundle.tx_hash.is_none(), "SEND_AND_CONFIRM_CALLS=0");
}

#[tokio::test]
async fn risk_rejection_yields_zero_executor_calls() {
    let (mut source, _mock) = source_with_mock();
    let cfg = Config::default();
    // net_pnl_atomic <= 0 on every round -> NonPositiveNetPnl on all three,
    // but the *aggregator* itself already requires positive economics, so
    // no window ever reaches "stable" and risk is never even reached this
    // way. To exercise risk rejection specifically (stable window reached,
    // but risk says no), keep economics positive for aggregation and make
    // the *risk* config impossibly strict instead.
    let anchors = [(200u64, 11u8), (203, 12), (206, 13)];
    let mut last = None;
    for (number, hash_byte) in anchors {
        let round_evidence = vec![evidence(
            number,
            hash_byte,
            99,
            MAIN_PENDING_DRY_RUN_PROFILE,
        )];
        let anchor = AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
        };
        last = Some(
            source
                .run_evidence_round(anchor, round_evidence, &cfg, number, true)
                .await,
        );
    }
    // Reconstruct with an impossibly strict gas ceiling to force rejection
    // on the round that would otherwise emit — done via a second source so
    // the strict config is exercised end to end.
    let (provider, _mock2) = Provider::<MockProvider>::mocked();
    let client = CanonicalSimulationClient::new(Arc::new(provider), a(99));
    let strict_cfg = CanonicalRiskConfig {
        max_gas_raw: U256::zero(),
        ..permissive_risk_cfg()
    };
    let mut strict_source = CanonicalC2BOpportunitySource::new(
        RiskManager::new(RiskConfig::default()),
        client,
        strict_cfg,
    );
    let mut strict_last = None;
    for (number, hash_byte) in anchors {
        let round_evidence = vec![evidence(
            number,
            hash_byte,
            99,
            MAIN_PENDING_DRY_RUN_PROFILE,
        )];
        let anchor = AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
        };
        strict_last = Some(
            strict_source
                .run_evidence_round(anchor, round_evidence, &cfg, number, true)
                .await,
        );
    }
    let strict_last = strict_last.unwrap();
    assert_eq!(strict_last.stable_opportunities.len(), 1);
    assert_eq!(strict_last.risk_approvals.len(), 1);
    assert!(
        strict_last.risk_approvals[0].1.is_err(),
        "risk must reject a zero gas ceiling"
    );
    assert!(strict_last.strategy_decisions.is_empty());
    assert!(
        strict_last.execution_results.is_empty(),
        "CANONICAL_EXECUTOR_INVOCATIONS=0"
    );

    // The permissive run's last round is untouched by the strict scenario.
    assert!(last.unwrap().execution_results.len() <= 1);
}

#[tokio::test]
async fn different_execution_profiles_never_aggregate_together() {
    let (mut source, _mock) = source_with_mock();
    let cfg = Config::default();
    // Same structural key/anchor cadence, but alternating operational
    // profile: fork-trace-audit and main-pending-dry-run must never share
    // a stability window even though everything else matches.
    let rounds = [
        (300u64, 21u8, MAIN_PENDING_DRY_RUN_PROFILE),
        (303, 22, FORK_TRACE_AUDIT_PROFILE),
        (306, 23, MAIN_PENDING_DRY_RUN_PROFILE),
    ];
    let mut last = None;
    for (number, hash_byte, profile_label) in rounds {
        let round_evidence = vec![evidence(number, hash_byte, 99, profile_label)];
        let anchor = AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
        };
        last = Some(
            source
                .run_evidence_round(anchor, round_evidence, &cfg, number, true)
                .await,
        );
    }
    assert!(
        last.unwrap().stable_opportunities.is_empty(),
        "mixed execution profiles must never aggregate into one opportunity"
    );
}

#[tokio::test]
async fn repeated_anchor_number_never_becomes_stable() {
    let (mut source, _mock) = source_with_mock();
    let cfg = Config::default();
    // Anchor 400 appears twice: not three *distinct* anchor blocks.
    let rounds = [(400u64, 31u8), (400, 31), (406, 33)];
    let mut last = None;
    for (number, hash_byte) in rounds {
        let round_evidence = vec![evidence(
            number,
            hash_byte,
            99,
            MAIN_PENDING_DRY_RUN_PROFILE,
        )];
        let anchor = AnchorBlock {
            number,
            hash: H256::repeat_byte(hash_byte),
            selected_from_head: number,
            confirmation_lag: 0,
        };
        last = Some(
            source
                .run_evidence_round(anchor, round_evidence, &cfg, number, true)
                .await,
        );
    }
    assert!(last.unwrap().stable_opportunities.is_empty());
}

#[tokio::test]
async fn unauthorized_caller_never_reaches_the_aggregator() {
    let (mut source, _mock) = source_with_mock();
    let cfg = Config::default();
    let anchor = AnchorBlock {
        number: 500,
        hash: H256::repeat_byte(41),
        selected_from_head: 500,
        confirmation_lag: 0,
    };
    let round_evidence = vec![evidence(500, 41, 99, MAIN_PENDING_DRY_RUN_PROFILE)];
    let result = source
        .run_evidence_round(anchor, round_evidence, &cfg, 500, false)
        .await;
    assert!(result.stable_opportunities.is_empty());
    assert!(result.execution_results.is_empty());
}
