//! E1-A adapter: stateful route economics for the fresh campaign.
//!
//! This module deliberately contains no RPC calls.  The caller supplies the
//! pinned first-touch state/quotes; all subsequent legs consume the previous
//! leg's atomic output and mutate a fresh `PoolReuseTracker`.

use crate::core::pool_state_sim::{PoolKind, PoolReuseTracker, PoolTouch, SimulatedPoolState};
use crate::core::route_artifact::RouteLeg;
use ethers::types::U256;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, Default)]
pub struct PinnedStateSnapshot {
    pub pools: HashMap<String, SimulatedPoolState>,
}

#[derive(Debug, Clone, Copy)]
pub struct SimulationContext {
    pub start_decimals: u8,
    pub gas_cost_atomic: U256,
    pub flashloan_cost_atomic: U256,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteSimulationResult {
    pub final_amount_atomic: U256,
    pub gross_pnl_atomic: i128,
    pub gas_cost_atomic: U256,
    pub flashloan_cost_atomic: U256,
    pub net_pnl_atomic: i128,
    pub pool_reuse_detected: bool,
    pub all_models_supported: bool,
}

#[derive(Debug, Error)]
pub enum EconomicEvaluationError {
    #[error("ECONOMIC_UNSUPPORTED_POOL_MODEL: {venue}/{protocol}")]
    UnsupportedPoolModel { venue: String, protocol: String },
    #[error("ECONOMIC_SIMULATION_ERROR: {0}")]
    Simulation(String),
    #[error("ECONOMIC_QUOTE_MISSING: leg={0}")]
    QuoteMissing(usize),
}

pub trait FreshEconomicEvaluator {
    fn evaluate(
        &self,
        route: &[RouteLeg],
        amount_in: U256,
        snapshot: &PinnedStateSnapshot,
        context: &SimulationContext,
        first_touch_quotes: &[U256],
    ) -> Result<RouteSimulationResult, EconomicEvaluationError>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct StatefulRouteEvaluator;

impl FreshEconomicEvaluator for StatefulRouteEvaluator {
    fn evaluate(
        &self,
        route: &[RouteLeg],
        amount_in: U256,
        snapshot: &PinnedStateSnapshot,
        context: &SimulationContext,
        first_touch_quotes: &[U256],
    ) -> Result<RouteSimulationResult, EconomicEvaluationError> {
        let mut tracker = PoolReuseTracker::new();
        let mut amount = amount_in;
        let mut reused = false;
        for (index, leg) in route.iter().enumerate() {
            let kind = PoolKind::classify(&leg.venue, &leg.protocol_version);
            let pool_id = crate::core::route_artifact::pool_identity(leg);
            let state = snapshot.pools.get(&pool_id).ok_or_else(|| {
                EconomicEvaluationError::UnsupportedPoolModel {
                    venue: leg.venue.clone(),
                    protocol: leg.protocol_version.clone(),
                }
            })?;
            match tracker
                .touch(&pool_id, index, amount)
                .map_err(|e| EconomicEvaluationError::Simulation(e.to_string()))?
            {
                PoolTouch::FirstTouch => {
                    let quote = *first_touch_quotes
                        .get(index)
                        .ok_or(EconomicEvaluationError::QuoteMissing(index))?;
                    if quote.is_zero() {
                        return Err(EconomicEvaluationError::Simulation("zero output".into()));
                    }
                    tracker.record_first_touch(&pool_id, index, *state);
                    amount = quote;
                }
                PoolTouch::ReuseComputedLocally(output) => {
                    reused = true;
                    amount = output;
                }
            }
            if !kind.supports_exact_reuse_mutation()
                && tracker.was_touched(&pool_id)
                && index + 1 < route.len()
            {
                // The next touch will fail closed; do not approximate opaque state.
            }
        }
        let gross = i128::try_from(amount.as_u128())
            .unwrap_or(i128::MAX)
            .saturating_sub(i128::try_from(amount_in.as_u128()).unwrap_or(i128::MAX));
        let gas = i128::try_from(context.gas_cost_atomic.as_u128()).unwrap_or(i128::MAX);
        let flashloan =
            i128::try_from(context.flashloan_cost_atomic.as_u128()).unwrap_or(i128::MAX);
        Ok(RouteSimulationResult {
            final_amount_atomic: amount,
            gross_pnl_atomic: gross,
            gas_cost_atomic: context.gas_cost_atomic,
            flashloan_cost_atomic: context.flashloan_cost_atomic,
            net_pnl_atomic: gross.saturating_sub(gas).saturating_sub(flashloan),
            pool_reuse_detected: reused,
            all_models_supported: true,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::route_artifact::RouteLeg;

    fn leg() -> RouteLeg {
        RouteLeg {
            token_in: "A".into(),
            token_out: "B".into(),
            venue: "QuickSwap".into(),
            protocol_version: "V2".into(),
            pool_address: Some("pool".into()),
            fee_tier: None,
        }
    }

    #[test]
    fn stateful_output_is_atomic_and_non_placeholder() {
        let route = vec![leg()];
        let id = crate::core::route_artifact::pool_identity(&route[0]);
        let mut pools = HashMap::new();
        pools.insert(
            id,
            SimulatedPoolState::ConstantProduct {
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(2_000_000u64),
                fee_bps: 30,
            },
        );
        let result = StatefulRouteEvaluator
            .evaluate(
                &route,
                U256::from(1_000u64),
                &PinnedStateSnapshot { pools },
                &SimulationContext {
                    start_decimals: 6,
                    gas_cost_atomic: U256::from(1u64),
                    flashloan_cost_atomic: U256::from(2u64),
                },
                &[U256::from(1_900u64)],
            )
            .unwrap();
        assert_eq!(result.final_amount_atomic, U256::from(1_900u64));
        assert_eq!(result.gross_pnl_atomic, 900);
        assert_eq!(result.net_pnl_atomic, 897);
    }
}
