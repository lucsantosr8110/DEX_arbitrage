//! Execution viability gate — Phase 2D-C2.
//!
//! Distinguishes "this route can be quoted" from "this route can be
//! executed".  Prevents quote-positive but execution-reverted routes from
//! reaching Phase 2D-D.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// How much evidence exists that a route can actually execute on-chain.
/// `QuoteOnly` is never sufficient for `FORK_CANDIDATE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum ExecutionEvidenceLevel {
    /// Route was only quoted via eth_call (get_dy, quoteExactInputSingle).
    QuoteOnly,
    /// Route was simulated via a stateful sequential model but never
    /// executed against real bytecode on a fork.
    StatefulMathSimulated,
    /// Each leg was verified via eth_call on the target contract (read-only
    /// call succeeded).
    ReadOnlyCallVerified,
    /// Each leg was executed on a local fork and produced a valid receipt.
    LocalForkLegVerified,
    /// The complete route (all legs in sequence) was executed on a local fork
    /// and produced a net-positive result.
    LocalForkRouteVerified,
    /// The route is known to deterministically revert under real bytecode.
    RejectedKnownRevert,
}

/// Per-leg execution capability. A route with any leg at
/// `UnsupportedStateMutation` or `KnownDeterministicRevert` cannot be a
/// fork candidate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LegExecutionCapability {
    ExecutableVerified,
    QuoteOnlyUnverified,
    StatefulSimulationOnly,
    UnsupportedStateMutation,
    KnownDeterministicRevert,
    InvalidCalldata,
    Unknown,
}

/// Categorisation of why the preflight reverted — only
/// `PreflightPass` allows FORK_CANDIDATE.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionPreflightOutcome {
    PreflightPass,
    PreflightRevert,
    PreflightInvalidCalldata,
    PreflightUnsupported,
    PreflightEnvironmentFailure,
}

impl ExecutionPreflightOutcome {
    pub fn label(self) -> &'static str {
        match self {
            Self::PreflightPass => "EXECUTION_PREFLIGHT_PASS",
            Self::PreflightRevert => "EXECUTION_PREFLIGHT_REVERT",
            Self::PreflightInvalidCalldata => "EXECUTION_PREFLIGHT_INVALID_CALLDATA",
            Self::PreflightUnsupported => "EXECUTION_PREFLIGHT_UNSUPPORTED",
            Self::PreflightEnvironmentFailure => "EXECUTION_PREFLIGHT_ENVIRONMENT_FAILURE",
        }
    }
}

/// A route that has been formally rejected from execution candidacy.
/// Persisted by `structural_cycle_key` so it cannot re-enter via a
/// different profile (base vs liquid).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedRoute {
    pub structural_cycle_key: String,
    pub reason: RouteRejectionReason,
    pub evidence_artifact: String,
    pub first_observed_block: u64,
    pub last_confirmed_block: u64,
    pub deterministic: bool,
    pub source_profiles: Vec<String>,
    pub token_path: Vec<String>,
    pub venue_path: Vec<String>,
    pub pool_path: Vec<String>,
}

/// Why a route was rejected. Each variant carries enough context to audit
/// the decision without re-running the failing test.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RouteRejectionReason {
    AaveReserveInactive { detail: String },
    AaveReserveFrozen { detail: String },
    AavePoolPaused { detail: String },
    AaveSupplyCapReached { detail: String },
    AaveDepositDisabled { detail: String },
    CurveMethodIncompatible { detail: String },
    CurveTokenIndexInvalid { detail: String },
    CurveImplementationDeprecated { detail: String },
    HistoricalProtocolIncompatibility { detail: String },
    InvalidCalldata { detail: String },
    WrongSelector { detail: String },
    UnsupportedPoolReuse { detail: String },
    LegRevertDuringPreflight { detail: String },
    UnknownDeterministicProtocolRevert { detail: String },
}

impl RouteRejectionReason {
    pub fn label(&self) -> &'static str {
        match self {
            Self::AaveReserveInactive { .. } => "AAVE_RESERVE_INACTIVE",
            Self::AaveReserveFrozen { .. } => "AAVE_RESERVE_FROZEN",
            Self::AavePoolPaused { .. } => "AAVE_POOL_PAUSED",
            Self::AaveSupplyCapReached { .. } => "AAVE_SUPPLY_CAP_REACHED",
            Self::AaveDepositDisabled { .. } => "AAVE_DEPOSIT_DISABLED",
            Self::CurveMethodIncompatible { .. } => "CURVE_METHOD_INCOMPATIBLE",
            Self::CurveTokenIndexInvalid { .. } => "CURVE_TOKEN_INDEX_INVALID",
            Self::CurveImplementationDeprecated { .. } => "CURVE_IMPLEMENTATION_DEPRECATED",
            Self::HistoricalProtocolIncompatibility { .. } => "HISTORICAL_PROTOCOL_INCOMPATIBILITY",
            Self::InvalidCalldata { .. } => "INVALID_CALLDATA",
            Self::WrongSelector { .. } => "WRONG_SELECTOR",
            Self::UnsupportedPoolReuse { .. } => "UNSUPPORTED_POOL_REUSE",
            Self::LegRevertDuringPreflight { .. } => "LEG_REVERT_DURING_PREFLIGHT",
            Self::UnknownDeterministicProtocolRevert { .. } => {
                "UNKNOWN_DETERMINISTIC_PROTOCOL_REVERT"
            }
        }
    }
}

/// The rejected-routes registry. Keyed by `structural_cycle_key`.
/// Once a route is registered, no profile (base, liquid, …) can
/// reintroduce it as a candidate.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RejectedRouteRegistry {
    /// structural_cycle_key -> RejectedRoute
    routes: BTreeMap<String, RejectedRoute>,
}

impl RejectedRouteRegistry {
    pub fn new() -> Self {
        Self {
            routes: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, route: RejectedRoute) {
        self.routes
            .insert(route.structural_cycle_key.clone(), route);
    }

    pub fn is_rejected(&self, structural_cycle_key: &str) -> bool {
        self.routes.contains_key(structural_cycle_key)
    }

    pub fn rejection(&self, structural_cycle_key: &str) -> Option<&RejectedRoute> {
        self.routes.get(structural_cycle_key)
    }

    pub fn all_rejected(&self) -> impl Iterator<Item = &RejectedRoute> {
        self.routes.values()
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

impl Default for RejectedRouteRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Summary of a single route's evaluation across economics, ABI,
/// and execution viability.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEvaluationVerdict {
    pub structural_cycle_key: String,
    pub token_path: Vec<String>,
    pub venues: Vec<String>,
    pub pool_ids: Vec<String>,
    pub source_profiles: Vec<String>,
    pub economic_classification: String,
    pub net_pnl_positive: bool,
    pub execution_evidence: ExecutionEvidenceLevel,
    pub preflight_outcome: Option<ExecutionPreflightOutcome>,
    pub fork_candidate: bool,
    pub exclusion_reason: Option<String>,
}

impl RouteEvaluationVerdict {
    pub fn economically_positive(&self) -> bool {
        self.net_pnl_positive
    }

    pub fn is_preflight_pass(&self) -> bool {
        self.preflight_outcome == Some(ExecutionPreflightOutcome::PreflightPass)
    }
}

/// Deduplication-aware counter.
pub struct CandidateCounts {
    pub raw_structural_routes: usize,
    pub unique_physical_routes: usize,
    pub raw_economic_candidates: usize,
    pub unique_economic_candidates: usize,
    pub raw_preflight_candidates: usize,
    pub unique_preflight_candidates: usize,
}

/// A leg's ABI validity record. Used in adapter audits.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdapterAbiAudit {
    pub adapter: String,
    pub router: String,
    pub method: String,
    pub canonical_signature: String,
    pub derived_selector: String,
    pub local_selector_before: Option<String>,
    pub local_selector_after: Option<String>,
    pub abi_valid_before: bool,
    pub abi_valid_after: bool,
    pub fork_eth_call_pass: Option<bool>,
    pub fork_transaction_pass: Option<bool>,
    pub status: String,
}

// ============================================================
// Pure logic helpers
// ============================================================

/// Checks whether a route is blocked by the rejected registry.
/// Returns `Some(reason)` if rejected, `None` if clear.
pub fn check_rejected<'a>(
    registry: &'a RejectedRouteRegistry,
    structural_cycle_key: &str,
) -> Option<&'a RejectedRoute> {
    registry.rejection(structural_cycle_key)
}

/// Determines the effective structural_cycle_key from a `{profile}:{key}`
/// route_id.  Returns `None` if the route_id has no colon separator.
pub fn structural_key_from_route_id(route_id: &str) -> Option<&str> {
    route_id.split_once(':').map(|(_, key)| key)
}

/// True only if the route can become a fork candidate.
pub fn is_fork_candidate_eligible(
    evidence: ExecutionEvidenceLevel,
    preflight: Option<ExecutionPreflightOutcome>,
    rejected: bool,
    economically_positive: bool,
) -> bool {
    if rejected {
        return false;
    }
    if !economically_positive {
        return false;
    }
    match evidence {
        ExecutionEvidenceLevel::QuoteOnly
        | ExecutionEvidenceLevel::StatefulMathSimulated
        | ExecutionEvidenceLevel::RejectedKnownRevert => return false,
        ExecutionEvidenceLevel::ReadOnlyCallVerified
        | ExecutionEvidenceLevel::LocalForkLegVerified
        | ExecutionEvidenceLevel::LocalForkRouteVerified => {}
    }
    match preflight {
        Some(ExecutionPreflightOutcome::PreflightPass) => true,
        Some(ExecutionPreflightOutcome::PreflightRevert)
        | Some(ExecutionPreflightOutcome::PreflightInvalidCalldata)
        | Some(ExecutionPreflightOutcome::PreflightUnsupported)
        | Some(ExecutionPreflightOutcome::PreflightEnvironmentFailure) => false,
        None => {
            // No preflight run — evidence must be LocalForkRouteVerified
            matches!(evidence, ExecutionEvidenceLevel::LocalForkRouteVerified)
        }
    }
}

// ============================================================
// Canonical selectors for known routers
// ============================================================

pub const UNISWAP_V3_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
pub const UNISWAP_V3_EXACT_INPUT_SINGLE_SIGNATURE: &str =
    "exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))";
pub const UNISWAP_V3_EXACT_INPUT_SINGLE_FLAT_SIGNATURE: &str =
    "exactInputSingle(address,address,uint24,address,uint256,uint256,uint256,uint160)";

/// Returns the 4-byte selector (as hex) for the canonical tuple-based
/// `exactInputSingle`.
pub fn uniswap_v3_canonical_selector() -> String {
    let hash = ethers::utils::keccak256(UNISWAP_V3_EXACT_INPUT_SINGLE_SIGNATURE.as_bytes());
    hex::encode(&hash[..4])
}

/// Returns the 4-byte selector (as hex) for the INCORRECT flat-argument
/// `exactInputSingle` signature.
pub fn uniswap_v3_flat_selector() -> String {
    let hash = ethers::utils::keccak256(UNISWAP_V3_EXACT_INPUT_SINGLE_FLAT_SIGNATURE.as_bytes());
    hex::encode(&hash[..4])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_and_flat_selectors_differ() {
        let canonical = uniswap_v3_canonical_selector();
        let flat = uniswap_v3_flat_selector();
        assert_ne!(
            canonical, flat,
            "canonical tuple selector must differ from flat-arg selector"
        );
    }

    #[test]
    fn canonical_selector_matches_real_router_interfaces() {
        // The canonical Uniswap V3 router (0xE592427A...) defines
        // exactInputSingle as taking a single tuple.  Known selector:
        // keccak256("exactInputSingle((address,address,uint24,address,uint256,uint256,uint256,uint160))")[0..4]
        // = 0x414bf389
        let canonical = uniswap_v3_canonical_selector();
        assert_eq!(canonical, "414bf389");
    }

    #[test]
    fn flat_selector_is_different() {
        let flat = uniswap_v3_flat_selector();
        // Must NOT equal the canonical selector.
        assert_ne!(flat, "414bf389");
        // keccak256("exactInputSingle(address,address,uint24,address,uint256,uint256,uint256,uint160)")[..4]
        assert_eq!(flat, "41060ae0");
    }

    #[test]
    fn rejected_route_cannot_be_fork_candidate() {
        let mut registry = RejectedRouteRegistry::new();
        registry.register(RejectedRoute {
            structural_cycle_key:
                "USDC>USDT|UniswapV3|V3||500||USDT>USDC|Curve|CurveStableSwap|0xpool|".into(),
            reason: RouteRejectionReason::HistoricalProtocolIncompatibility {
                detail: "Aave V2 LendingPool.deposit() reverted in 3/3 anchor blocks".into(),
            },
            evidence_artifact: "diagnostics/phase2d_d_failures_20260730T182714Z.jsonl".into(),
            first_observed_block: 91149850,
            last_confirmed_block: 91149916,
            deterministic: true,
            source_profiles: vec!["base".into(), "liquid".into()],
            token_path: vec!["USDC".into(), "USDT".into(), "USDC".into()],
            venue_path: vec!["UniswapV3".into(), "Curve".into()],
            pool_path: vec!["0xE592427A...".into(), "0x445FE580...".into()],
        });
        assert!(registry
            .is_rejected("USDC>USDT|UniswapV3|V3||500||USDT>USDC|Curve|CurveStableSwap|0xpool|"));
        assert!(!registry.is_rejected("some_other_key"));
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn structural_key_from_route_id_works() {
        assert_eq!(
            structural_key_from_route_id("base:USDC>USDT|UniswapV3|500|"),
            Some("USDC>USDT|UniswapV3|500|")
        );
        assert_eq!(structural_key_from_route_id("no-separator"), None);
    }

    #[test]
    fn is_fork_candidate_eligible_rejects_quote_only() {
        assert!(!is_fork_candidate_eligible(
            ExecutionEvidenceLevel::QuoteOnly,
            None,
            false,
            true,
        ));
    }

    #[test]
    fn is_fork_candidate_eligible_requires_economic_positivity() {
        assert!(!is_fork_candidate_eligible(
            ExecutionEvidenceLevel::LocalForkRouteVerified,
            Some(ExecutionPreflightOutcome::PreflightPass),
            false,
            false,
        ));
    }

    #[test]
    fn is_fork_candidate_eligible_rejects_rejected_routes() {
        assert!(!is_fork_candidate_eligible(
            ExecutionEvidenceLevel::LocalForkRouteVerified,
            Some(ExecutionPreflightOutcome::PreflightPass),
            true,
            true,
        ));
    }

    #[test]
    fn is_fork_candidate_eligible_accepts_preflight_pass_with_proper_evidence() {
        assert!(is_fork_candidate_eligible(
            ExecutionEvidenceLevel::ReadOnlyCallVerified,
            Some(ExecutionPreflightOutcome::PreflightPass),
            false,
            true,
        ));
    }

    #[test]
    fn is_fork_candidate_eligible_rejects_preflight_revert() {
        assert!(!is_fork_candidate_eligible(
            ExecutionEvidenceLevel::ReadOnlyCallVerified,
            Some(ExecutionPreflightOutcome::PreflightRevert),
            false,
            true,
        ));
    }

    #[test]
    fn is_fork_candidate_eligible_local_fork_route_without_preflight_still_ok() {
        assert!(is_fork_candidate_eligible(
            ExecutionEvidenceLevel::LocalForkRouteVerified,
            None,
            false,
            true,
        ));
    }
}
