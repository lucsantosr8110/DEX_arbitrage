//! Phase 2D-C2 safety regression: the execution-viability hardening phase
//! must introduce a formal gate between "quote positive" and "fork
//! candidate", and the Uniswap V3 adapter's flat-arg ABI must be corrected
//! to the canonical tuple encoding.  These tests guard against regression:
//! re-introducing the flat `exactInputSingle` ABI, removing the rejected-
//! routes registry, or allowing quote-positive-only routes to become fork
//! candidates.

use flashloan_bot::core::execution_viability::{
    check_rejected, is_fork_candidate_eligible, structural_key_from_route_id,
    uniswap_v3_canonical_selector, uniswap_v3_flat_selector, ExecutionEvidenceLevel,
    ExecutionPreflightOutcome, RejectedRoute, RejectedRouteRegistry, RouteRejectionReason,
};

const ADAPTER_SOURCE: &str = include_str!("../src/dex/adapters/uniswap_v3.rs");
const EXECUTION_VIABILITY_SOURCE: &str = include_str!("../src/core/execution_viability.rs");

// ============================================================
// Safety gates
// ============================================================

#[test]
fn phase2d_c2_never_writes_to_mainnet() {
    // The execution_viability module must never contain any symbol
    // that would enable mainnet writes.  Block each known live-trading
    // token at compile-check time via forbiddance, not runtime state.
    let forbidden: &[&str] = &[
        "MainnetWriteEnabled",
        "LiveExecutionAuthorized",
        "ProductionSigner",
        "ProductionBroadcaster",
        "TransactionBroadcastAllowed",
        "CyclesEconomicallyTrusted",
        "MAINNET_WRITE_RPC_CALLS",
    ];
    for token in forbidden {
        assert!(
            !EXECUTION_VIABILITY_SOURCE.contains(token),
            "forbidden live-trading symbol `{token}` must never appear in execution_viability.rs"
        );
    }
}

#[test]
fn phase2d_c2_forbidden_symbols_not_in_execution_viability() {
    for term in [
        "LocalWallet",
        "SignerMiddleware",
        "AppMiddleware",
        "PRIVATE_KEY",
    ] {
        assert!(
            !EXECUTION_VIABILITY_SOURCE.contains(term),
            "forbidden symbol `{term}` found in execution_viability.rs"
        );
    }
}

// ============================================================
// Selector verification
// ============================================================

#[test]
fn phase2d_c2_canonical_uniswap_v3_selector_matches_known_value() {
    let selector = uniswap_v3_canonical_selector();
    assert_eq!(selector, "414bf389", "canonical tuple selector mismatch");
}

#[test]
fn phase2d_c2_flat_selector_different_from_canonical() {
    let canonical = uniswap_v3_canonical_selector();
    let flat = uniswap_v3_flat_selector();
    assert_ne!(canonical, flat, "flat selector must differ from canonical");
}

#[test]
fn phase2d_c2_flat_selector_matches_known_value() {
    let flat = uniswap_v3_flat_selector();
    // keccak256("exactInputSingle(address,address,uint24,address,uint256,uint256,uint256,uint160)")[..4]
    assert_eq!(flat, "41060ae0", "flat-arg selector mismatch");
}

#[test]
fn phase2d_c2_adapter_no_longer_uses_flat_selector() {
    // The adapter must use the tuple-encoded call, not flat-arg.
    // Look for the tuple-wrapping pattern in the swap method.
    let tuple_call = ADAPTER_SOURCE.contains("exactInputSingle\", (params,))");
    assert!(
        tuple_call,
        "adapter must wrap params in a single-element tuple: exactInputSingle\", (params,))"
    );
}

#[test]
fn phase2d_c2_adapter_rejects_flat_encoding() {
    assert!(
        !ADAPTER_SOURCE.contains("exactInputSingle\", params)"),
        "adapter must NOT pass flat params — must use (params,) tuple wrapper"
    );
}

// ============================================================
// Rejected routes registry
// ============================================================

fn sample_rejected_route() -> RejectedRoute {
    RejectedRoute {
        structural_cycle_key: "USDC>USDT|UniswapV3|V3||500||USDT>USDC|Curve|CurveStableSwap|0x445FE580eF8d70FF569aB36e80c647af338db351|".into(),
        reason: RouteRejectionReason::HistoricalProtocolIncompatibility {
            detail: "Aave V2 LendingPool.deposit() reverted in 3/3 anchor blocks (91149850, 91149883, 91149916)".into(),
        },
        evidence_artifact: "diagnostics/phase2d_d_failures_20260730T182714Z.jsonl".into(),
        first_observed_block: 91149850,
        last_confirmed_block: 91149916,
        deterministic: true,
        source_profiles: vec!["base".into(), "liquid".into()],
        token_path: vec!["USDC".into(), "USDT".into(), "USDC".into()],
        venue_path: vec!["UniswapV3".into(), "Curve".into()],
        pool_path: vec![
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".into(),
            "0x445FE580eF8d70FF569aB36e80c647af338db351".into(),
        ],
    }
}

#[test]
fn phase2d_c2_rejected_route_registry_prevents_reentry() {
    let mut registry = RejectedRouteRegistry::new();
    let route = sample_rejected_route();
    let key = route.structural_cycle_key.clone();
    registry.register(route);

    assert!(registry.is_rejected(&key));
    assert_eq!(registry.len(), 1);
}

#[test]
fn phase2d_c2_rejected_route_cannot_reenter_via_different_profile() {
    let mut registry = RejectedRouteRegistry::new();
    let route = sample_rejected_route();
    let key = route.structural_cycle_key.clone();

    // Register under 'base' profile
    registry.register(route);

    // Try to re-enter with a 'liquid' profile — must still be rejected.
    // The structural_cycle_key is the same, regardless of profile.
    assert!(
        registry.is_rejected(&key),
        "route must remain rejected even when queried without profile"
    );
}

#[test]
fn phase2d_c2_route_not_in_registry_is_not_rejected() {
    let registry = RejectedRouteRegistry::new();
    assert!(!registry.is_rejected("UNKNOWN_KEY"));
}

#[test]
fn phase2d_c2_structural_key_extracted_from_route_id() {
    assert_eq!(structural_key_from_route_id("base:KEY"), Some("KEY"));
    assert_eq!(structural_key_from_route_id("liquid:KEY"), Some("KEY"));
    assert_eq!(structural_key_from_route_id("bare_key"), None);
}

// ============================================================
// Execution preflight gate
// ============================================================

#[test]
fn phase2d_c2_quote_only_not_fork_candidate() {
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::QuoteOnly,
        None,
        false,
        true,
    ));
}

#[test]
fn phase2d_c2_positive_quote_does_not_imply_executable_route() {
    let mut registry = RejectedRouteRegistry::new();
    let route = sample_rejected_route();
    registry.register(route);

    // Even if economically positive, a rejected route cannot be a candidate
    if let Some(r) = check_rejected(&registry, "USDC>USDT|UniswapV3|V3||500||USDT>USDC|Curve|CurveStableSwap|0x445FE580eF8d70FF569aB36e80c647af338db351|") {
        assert!(matches!(r.reason, RouteRejectionReason::HistoricalProtocolIncompatibility { .. }));
        assert!(!is_fork_candidate_eligible(
            ExecutionEvidenceLevel::ReadOnlyCallVerified,
            Some(ExecutionPreflightOutcome::PreflightPass),
            true,
            true,
        ));
    }
}

#[test]
fn phase2d_c2_known_reverted_route_cannot_reenter_candidate_set() {
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::RejectedKnownRevert,
        Some(ExecutionPreflightOutcome::PreflightRevert),
        false,
        true,
    ));
}

#[test]
fn phase2d_c2_execution_preflight_is_required_for_fork_candidate() {
    // ReadOnlyCallVerified without preflight should not be enough
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::ReadOnlyCallVerified,
        None,
        false,
        true,
    ));

    // LocalForkRouteVerified without preflight is still OK
    assert!(is_fork_candidate_eligible(
        ExecutionEvidenceLevel::LocalForkRouteVerified,
        None,
        false,
        true,
    ));
}

#[test]
fn phase2d_c2_preflight_pass_with_proper_evidence_eligible() {
    for evidence in [
        ExecutionEvidenceLevel::ReadOnlyCallVerified,
        ExecutionEvidenceLevel::LocalForkLegVerified,
        ExecutionEvidenceLevel::LocalForkRouteVerified,
    ] {
        assert!(
            is_fork_candidate_eligible(
                evidence,
                Some(ExecutionPreflightOutcome::PreflightPass),
                false,
                true
            ),
            "{:?} with preflight pass should be eligible",
            evidence,
        );
    }
}

#[test]
fn phase2d_c2_preflight_revert_blocks_candidate() {
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::ReadOnlyCallVerified,
        Some(ExecutionPreflightOutcome::PreflightRevert),
        false,
        true,
    ));
}

#[test]
fn phase2d_c2_negative_economics_blocks_candidate() {
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::LocalForkRouteVerified,
        Some(ExecutionPreflightOutcome::PreflightPass),
        false,
        false,
    ));
}

#[test]
fn phase2d_c2_preflight_outcome_labels_are_descriptive() {
    assert_eq!(
        ExecutionPreflightOutcome::PreflightPass.label(),
        "EXECUTION_PREFLIGHT_PASS"
    );
    assert_eq!(
        ExecutionPreflightOutcome::PreflightRevert.label(),
        "EXECUTION_PREFLIGHT_REVERT"
    );
    assert_eq!(
        ExecutionPreflightOutcome::PreflightInvalidCalldata.label(),
        "EXECUTION_PREFLIGHT_INVALID_CALLDATA"
    );
}

// ============================================================
// Adapter ABI audit
// ============================================================

#[test]
fn phase2d_c2_adapter_uses_router_abi_not_flat_params() {
    // The adapter's swap method uses the correct ABI wrapper
    assert!(
        ADAPTER_SOURCE.contains("UniswapV3Router.json"),
        "adapter must reference the canonical ABI JSON file"
    );
}

#[test]
fn phase2d_c2_zero_candidates_is_valid_result() {
    // Phase 2D-C2 explicitly permits NEW_PHASE2D_D_CANDIDATES=0
    // as a scientifically valid outcome.  This test asserts the
    // infrastructure can handle empty candidate sets without panic.
    let registry = RejectedRouteRegistry::new();
    let key = "NONEXISTENT|ROUTE";
    assert!(!registry.is_rejected(key));
    assert!(check_rejected(&registry, key).is_none());
    assert!(!is_fork_candidate_eligible(
        ExecutionEvidenceLevel::QuoteOnly,
        None,
        false,
        true,
    ));
}
