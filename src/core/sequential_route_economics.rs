//! Phase 2D-C economic classification: per-round outcome -> cross-round
//! (route, size) classification, and capacity-boundary reporting. Pure
//! logic — no RPC, no execution.

use serde::Serialize;

/// What happened for one (round, route, size) simulation attempt. This is
/// intentionally coarser than the full leg-level diagnostic record — it is
/// only the input to cross-round aggregation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoundOutcome {
    RouteInvalid,
    SimulationError,
    TemporallyIncoherent,
    UnsupportedReusedPool,
    GrossNegative,
    /// Gross positive, but gas (or the pinned MATIC->start-token conversion
    /// needed to net it against gas) could not be modeled for this round.
    GrossPositiveNetUnavailable,
    /// Gross positive, gas was modeled, but net turned negative — the
    /// route's own slippage/fees/gas ate the marginal-rate profit.
    GrossPositiveNetNegative,
    NetPositive,
}

/// Final classification for a (route, size) pair after all rounds. Only
/// `SequentiallyNetPositiveStable` may be proposed as a Phase 2D-D fork
/// candidate — and even then, `CYCLES_ECONOMICALLY_TRUSTED` and
/// `LIVE_EXECUTION_AUTHORIZED` stay `false`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum RouteSizeClassification {
    SequentiallyNetPositiveStable,
    SequentiallyGrossPositiveGasNegative,
    SequentiallyPositiveUnstable,
    SlippageNegative,
    GrossNegative,
    TemporallyIncoherent,
    #[allow(dead_code)]
    UnsupportedPoolModel,
    UnsupportedReusedPool,
    #[allow(dead_code)]
    UsdConversionUnavailable,
    GasUnavailable,
    RouteInvalid,
    SimulationError,
}

impl RouteSizeClassification {
    pub fn label(self) -> &'static str {
        match self {
            Self::SequentiallyNetPositiveStable => "SEQUENTIALLY_NET_POSITIVE_STABLE",
            Self::SequentiallyGrossPositiveGasNegative => {
                "SEQUENTIALLY_GROSS_POSITIVE_GAS_NEGATIVE"
            }
            Self::SequentiallyPositiveUnstable => "SEQUENTIALLY_POSITIVE_UNSTABLE",
            Self::SlippageNegative => "SLIPPAGE_NEGATIVE",
            Self::GrossNegative => "GROSS_NEGATIVE",
            Self::TemporallyIncoherent => "TEMPORALLY_INCOHERENT",
            Self::UnsupportedPoolModel => "UNSUPPORTED_POOL_MODEL",
            Self::UnsupportedReusedPool => "UNSUPPORTED_REUSED_POOL",
            Self::UsdConversionUnavailable => "USD_CONVERSION_UNAVAILABLE",
            Self::GasUnavailable => "GAS_UNAVAILABLE",
            Self::RouteInvalid => "ROUTE_INVALID",
            Self::SimulationError => "SIMULATION_ERROR",
        }
    }
}

/// Aggregates one (route, size)'s outcomes across all rounds into a single
/// classification. Order of checks matters: earlier checks are hard
/// blockers that pre-empt weaker/mixed signals below them.
pub fn aggregate_final_classification(rounds: &[RoundOutcome]) -> RouteSizeClassification {
    if rounds.is_empty() || rounds.contains(&RoundOutcome::RouteInvalid) {
        return RouteSizeClassification::RouteInvalid;
    }
    if rounds.contains(&RoundOutcome::SimulationError) {
        return RouteSizeClassification::SimulationError;
    }
    if rounds.contains(&RoundOutcome::TemporallyIncoherent) {
        return RouteSizeClassification::TemporallyIncoherent;
    }
    if rounds.contains(&RoundOutcome::UnsupportedReusedPool) {
        return RouteSizeClassification::UnsupportedReusedPool;
    }
    if rounds.iter().all(|r| *r == RoundOutcome::GrossNegative) {
        return RouteSizeClassification::GrossNegative;
    }
    if rounds.iter().all(|r| *r == RoundOutcome::NetPositive) {
        return RouteSizeClassification::SequentiallyNetPositiveStable;
    }
    if rounds.contains(&RoundOutcome::GrossNegative) {
        // Mixed positive/negative gross sign across independent anchor
        // blocks: not stable, regardless of any single round looking good.
        return RouteSizeClassification::SequentiallyPositiveUnstable;
    }
    if rounds
        .iter()
        .all(|r| *r == RoundOutcome::GrossPositiveNetUnavailable)
    {
        return RouteSizeClassification::SequentiallyGrossPositiveGasNegative;
    }
    if rounds.contains(&RoundOutcome::NetPositive)
        && rounds
            .iter()
            .all(|r| *r != RoundOutcome::GrossPositiveNetNegative)
    {
        // Some rounds net positive, others only gas-unavailable (never
        // outright net-negative): still not a stable 3/3 net-positive claim.
        return RouteSizeClassification::SequentiallyPositiveUnstable;
    }
    if rounds.contains(&RoundOutcome::GrossPositiveNetNegative)
        && rounds.iter().all(|r| *r != RoundOutcome::NetPositive)
    {
        return RouteSizeClassification::SlippageNegative;
    }
    RouteSizeClassification::SequentiallyPositiveUnstable
}

/// `true` only when the classification and per-round evidence together
/// satisfy every Phase 2D-D fork-candidate precondition in spec section 20.
/// `fork_candidate=true` never implies execution authorization.
pub fn is_fork_candidate(classification: RouteSizeClassification, rounds: &[RoundOutcome]) -> bool {
    classification == RouteSizeClassification::SequentiallyNetPositiveStable
        && rounds.len() >= 3
        && rounds.iter().all(|r| *r == RoundOutcome::NetPositive)
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct RoundPnl {
    pub size_human: f64,
    pub gross_positive: bool,
    pub net_positive: Option<bool>,
}

/// Largest tested size (in the campaign's chosen denomination) that stayed
/// positive across the whole grid — never extrapolated beyond the sizes
/// actually tested. `net_positive: None` (net unavailable) does not count as
/// a positive observation for the *net* boundary, only for the gross one.
#[derive(Debug, Clone, Copy, Default, Serialize)]
pub struct CapacityBoundary {
    pub largest_gross_positive_size: Option<f64>,
    pub largest_net_positive_size: Option<f64>,
    pub first_negative_size: Option<f64>,
    /// `true` if every tested size (up to the grid ceiling) stayed positive —
    /// i.e. the upper bound of capacity was not observed within the grid.
    pub capacity_upper_bound_unknown: bool,
}

/// `sizes_ascending` must already be sorted ascending; this function does
/// not sort defensively so the caller's grid order is preserved verbatim in
/// any error it might otherwise mask.
pub fn capacity_boundary(sizes_ascending: &[RoundPnl]) -> CapacityBoundary {
    let mut boundary = CapacityBoundary::default();
    for entry in sizes_ascending {
        if entry.gross_positive {
            boundary.largest_gross_positive_size = Some(entry.size_human);
        } else if boundary.first_negative_size.is_none() {
            boundary.first_negative_size = Some(entry.size_human);
        }
        if entry.net_positive == Some(true) {
            boundary.largest_net_positive_size = Some(entry.size_human);
        }
    }
    boundary.capacity_upper_bound_unknown = sizes_ascending
        .last()
        .is_some_and(|last| last.gross_positive)
        && boundary.first_negative_size.is_none();
    boundary
}

#[cfg(test)]
mod tests {
    use super::*;
    use RoundOutcome::*;

    #[test]
    fn all_three_net_positive_is_stable() {
        let c = aggregate_final_classification(&[NetPositive, NetPositive, NetPositive]);
        assert_eq!(c, RouteSizeClassification::SequentiallyNetPositiveStable);
        assert!(is_fork_candidate(
            c,
            &[NetPositive, NetPositive, NetPositive]
        ));
    }

    #[test]
    fn one_negative_round_breaks_stability_even_if_average_positive() {
        // Regression guard for "não usar média positiva para esconder uma
        // rodada negativa" (spec section 18).
        let rounds = [NetPositive, NetPositive, GrossPositiveNetNegative];
        let c = aggregate_final_classification(&rounds);
        assert_ne!(c, RouteSizeClassification::SequentiallyNetPositiveStable);
        assert!(!is_fork_candidate(c, &rounds));
    }

    #[test]
    fn all_gross_negative_is_gross_negative() {
        let c = aggregate_final_classification(&[GrossNegative, GrossNegative, GrossNegative]);
        assert_eq!(c, RouteSizeClassification::GrossNegative);
    }

    #[test]
    fn mixed_gross_sign_is_unstable() {
        let c = aggregate_final_classification(&[NetPositive, GrossNegative, NetPositive]);
        assert_eq!(c, RouteSizeClassification::SequentiallyPositiveUnstable);
    }

    #[test]
    fn any_route_invalid_short_circuits_everything() {
        let c = aggregate_final_classification(&[NetPositive, RouteInvalid, NetPositive]);
        assert_eq!(c, RouteSizeClassification::RouteInvalid);
    }

    #[test]
    fn any_simulation_error_short_circuits() {
        let c = aggregate_final_classification(&[NetPositive, SimulationError, NetPositive]);
        assert_eq!(c, RouteSizeClassification::SimulationError);
    }

    #[test]
    fn any_temporal_incoherence_short_circuits() {
        let c = aggregate_final_classification(&[NetPositive, TemporallyIncoherent, NetPositive]);
        assert_eq!(c, RouteSizeClassification::TemporallyIncoherent);
    }

    #[test]
    fn any_unsupported_reused_pool_short_circuits() {
        let c = aggregate_final_classification(&[NetPositive, UnsupportedReusedPool, NetPositive]);
        assert_eq!(c, RouteSizeClassification::UnsupportedReusedPool);
    }

    #[test]
    fn consistently_gas_unavailable_is_gross_positive_gas_negative() {
        let c = aggregate_final_classification(&[
            GrossPositiveNetUnavailable,
            GrossPositiveNetUnavailable,
            GrossPositiveNetUnavailable,
        ]);
        assert_eq!(
            c,
            RouteSizeClassification::SequentiallyGrossPositiveGasNegative
        );
    }

    #[test]
    fn consistently_slippage_negative_when_never_net_positive() {
        let c = aggregate_final_classification(&[
            GrossPositiveNetNegative,
            GrossPositiveNetNegative,
            GrossPositiveNetNegative,
        ]);
        assert_eq!(c, RouteSizeClassification::SlippageNegative);
    }

    #[test]
    fn empty_rounds_is_route_invalid_fail_closed() {
        assert_eq!(
            aggregate_final_classification(&[]),
            RouteSizeClassification::RouteInvalid
        );
    }

    #[test]
    fn fork_candidate_requires_at_least_3_rounds() {
        assert!(!is_fork_candidate(
            RouteSizeClassification::SequentiallyNetPositiveStable,
            &[NetPositive, NetPositive]
        ));
    }

    #[test]
    fn capacity_boundary_never_extrapolates_past_grid() {
        let sizes = [
            RoundPnl {
                size_human: 10.0,
                gross_positive: true,
                net_positive: Some(true),
            },
            RoundPnl {
                size_human: 100.0,
                gross_positive: true,
                net_positive: Some(true),
            },
            RoundPnl {
                size_human: 1000.0,
                gross_positive: true,
                net_positive: Some(true),
            },
        ];
        let boundary = capacity_boundary(&sizes);
        assert_eq!(boundary.largest_gross_positive_size, Some(1000.0));
        assert_eq!(boundary.largest_net_positive_size, Some(1000.0));
        assert!(boundary.first_negative_size.is_none());
        assert!(
            boundary.capacity_upper_bound_unknown,
            "all tested sizes positive -> upper bound must be UNKNOWN, not asserted as 1000"
        );
    }

    #[test]
    fn capacity_boundary_finds_first_negative_size() {
        let sizes = [
            RoundPnl {
                size_human: 10.0,
                gross_positive: true,
                net_positive: Some(true),
            },
            RoundPnl {
                size_human: 100.0,
                gross_positive: false,
                net_positive: Some(false),
            },
        ];
        let boundary = capacity_boundary(&sizes);
        assert_eq!(boundary.largest_gross_positive_size, Some(10.0));
        assert_eq!(boundary.first_negative_size, Some(100.0));
        assert!(!boundary.capacity_upper_bound_unknown);
    }

    #[test]
    fn capacity_boundary_net_unavailable_does_not_count_as_net_positive() {
        let sizes = [RoundPnl {
            size_human: 10.0,
            gross_positive: true,
            net_positive: None,
        }];
        let boundary = capacity_boundary(&sizes);
        assert_eq!(boundary.largest_gross_positive_size, Some(10.0));
        assert_eq!(boundary.largest_net_positive_size, None);
    }
}
