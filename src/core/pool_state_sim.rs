//! Stateful per-pool simulation for Phase 2D-C.
//!
//! Pure logic — no RPC. The caller (the campaign binary) is the only place
//! that touches the network: it asks this module what to do for a pool via
//! [`PoolReuseTracker::touch`], performs an on-chain pinned quote only when
//! told [`PoolTouch::FirstTouch`], and hands the result back via
//! [`PoolReuseTracker::record_first_touch`]. On a second use of the *same*
//! physical pool within the same route, this module either recomputes the
//! swap exactly against the already-mutated local state (constant-product
//! pools) or refuses with [`SimulationError::UnsupportedReusedPool`] — it
//! never re-issues the same on-chain call against the original, unmutated
//! block state, which would silently ignore the first leg's effect.
#![deny(clippy::arithmetic_side_effects)]

use ethers::types::U256;
use std::collections::HashMap;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum PoolKind {
    /// x*y=k with a flat fee taken from `amount_in`. Exact reuse mutation
    /// supported.
    UniswapV2ConstantProduct,
    /// Concentrated liquidity (ticks/sqrtPriceX96). No local exact simulator
    /// in this phase — reuse is fail-closed.
    UniswapV3Concentrated,
    /// StableSwap invariant (amplification-weighted). No local exact
    /// simulator in this phase — reuse is fail-closed.
    CurveStableSwap,
}

impl PoolKind {
    pub fn classify(venue: &str, protocol_version: &str) -> Self {
        if venue.eq_ignore_ascii_case("Curve") {
            return PoolKind::CurveStableSwap;
        }
        if protocol_version.eq_ignore_ascii_case("V3") {
            return PoolKind::UniswapV3Concentrated;
        }
        PoolKind::UniswapV2ConstantProduct
    }

    pub fn supports_exact_reuse_mutation(self) -> bool {
        matches!(self, PoolKind::UniswapV2ConstantProduct)
    }

    pub fn label(self) -> &'static str {
        match self {
            PoolKind::UniswapV2ConstantProduct => "UniswapV2ConstantProduct",
            PoolKind::UniswapV3Concentrated => "UniswapV3Concentrated",
            PoolKind::CurveStableSwap => "CurveStableSwap",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SimulatedPoolState {
    ConstantProduct {
        reserve_in: U256,
        reserve_out: U256,
        fee_bps: u32,
    },
    /// First touch was a real on-chain quote, but this phase has no local
    /// exact model to mutate its state for a hypothetical second touch.
    Opaque(PoolKind),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SimulationError {
    #[error("UNSUPPORTED_REUSED_POOL: pool_id={pool_id} leg_index={leg_index}")]
    UnsupportedReusedPool { pool_id: String, leg_index: usize },
    #[error("SIMULATION_ZERO_OUTPUT: pool_id={pool_id} leg_index={leg_index}")]
    ZeroOutput { pool_id: String, leg_index: usize },
    #[error("SIMULATION_OVERFLOW: pool_id={pool_id} leg_index={leg_index} detail={detail}")]
    Overflow {
        pool_id: String,
        leg_index: usize,
        detail: String,
    },
    #[error("SIMULATION_INSUFFICIENT_LIQUIDITY: pool_id={pool_id} leg_index={leg_index}")]
    InsufficientLiquidity { pool_id: String, leg_index: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PoolTouch {
    /// Not seen before in this route/size/round simulation. Caller must
    /// perform the real pinned on-chain quote, then call
    /// [`PoolReuseTracker::record_first_touch`].
    FirstTouch,
    /// Seen before; state was mutable and exact — `amount_out` is already
    /// computed against the post-first-leg state. No RPC call needed (or
    /// permitted: reusing the original on-chain quote here would be wrong).
    ReuseComputedLocally(U256),
}

/// Uniswap-V2-style constant-product output, floor-rounded, fee taken from
/// `amount_in`. `fee_bps` is out of 10_000 (e.g. 30 == 0.30%).
pub fn constant_product_amount_out(
    amount_in: U256,
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
    pool_id: &str,
    leg_index: usize,
) -> Result<U256, SimulationError> {
    if reserve_in.is_zero() || reserve_out.is_zero() {
        return Err(SimulationError::InsufficientLiquidity {
            pool_id: pool_id.to_string(),
            leg_index,
        });
    }
    let overflow = || SimulationError::Overflow {
        pool_id: pool_id.to_string(),
        leg_index,
        detail: "constant_product_amount_out".to_string(),
    };
    let fee_bps = U256::from(fee_bps);
    let ten_k = U256::from(10_000u32);
    let fee_multiplier = ten_k.checked_sub(fee_bps).ok_or_else(overflow)?;
    let amount_in_with_fee = amount_in.checked_mul(fee_multiplier).ok_or_else(overflow)?;
    let numerator = amount_in_with_fee
        .checked_mul(reserve_out)
        .ok_or_else(overflow)?;
    let scaled_reserve_in = reserve_in.checked_mul(ten_k).ok_or_else(overflow)?;
    let denominator = scaled_reserve_in
        .checked_add(amount_in_with_fee)
        .ok_or_else(overflow)?;
    if denominator.is_zero() {
        return Err(overflow());
    }
    let amount_out = numerator.checked_div(denominator).ok_or_else(overflow)?;
    if amount_out.is_zero() {
        return Err(SimulationError::ZeroOutput {
            pool_id: pool_id.to_string(),
            leg_index,
        });
    }
    if amount_out >= reserve_out {
        return Err(SimulationError::InsufficientLiquidity {
            pool_id: pool_id.to_string(),
            leg_index,
        });
    }
    Ok(amount_out)
}

fn constant_product_apply(
    reserve_in: U256,
    reserve_out: U256,
    amount_in: U256,
    amount_out: U256,
) -> (U256, U256) {
    (
        reserve_in.saturating_add(amount_in),
        reserve_out.saturating_sub(amount_out),
    )
}

#[derive(Debug, Default)]
pub struct PoolReuseTracker {
    states: HashMap<String, SimulatedPoolState>,
    /// pool_id -> leg_index of first touch, for reporting `pool_reuse_index`.
    first_touch_leg: HashMap<String, usize>,
}

impl PoolReuseTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn was_touched(&self, pool_id: &str) -> bool {
        self.states.contains_key(pool_id)
    }

    pub fn first_touch_leg_index(&self, pool_id: &str) -> Option<usize> {
        self.first_touch_leg.get(pool_id).copied()
    }

    /// Marginal reference rate implied by `pool_id`'s currently-tracked
    /// state, *before* applying this leg's swap. `None` if the pool hasn't
    /// been touched yet (no local state to derive a rate from — the caller
    /// should use an on-chain marginal probe instead) or its state is
    /// opaque.
    pub fn peek_reference_rate(&self, pool_id: &str) -> Option<f64> {
        match self.states.get(pool_id)? {
            SimulatedPoolState::ConstantProduct {
                reserve_in,
                reserve_out,
                ..
            } => constant_product_reference_rate(*reserve_in, *reserve_out),
            SimulatedPoolState::Opaque(_) => None,
        }
    }

    /// Decides whether `pool_id` needs a fresh on-chain quote for this leg,
    /// or whether its state can be mutated locally.
    pub fn touch(
        &mut self,
        pool_id: &str,
        leg_index: usize,
        amount_in: U256,
    ) -> Result<PoolTouch, SimulationError> {
        match self.states.get(pool_id).copied() {
            None => Ok(PoolTouch::FirstTouch),
            Some(SimulatedPoolState::Opaque(_)) => Err(SimulationError::UnsupportedReusedPool {
                pool_id: pool_id.to_string(),
                leg_index,
            }),
            Some(SimulatedPoolState::ConstantProduct {
                reserve_in,
                reserve_out,
                fee_bps,
            }) => {
                let amount_out = constant_product_amount_out(
                    amount_in,
                    reserve_in,
                    reserve_out,
                    fee_bps,
                    pool_id,
                    leg_index,
                )?;
                let (new_in, new_out) =
                    constant_product_apply(reserve_in, reserve_out, amount_in, amount_out);
                self.states.insert(
                    pool_id.to_string(),
                    SimulatedPoolState::ConstantProduct {
                        reserve_in: new_in,
                        reserve_out: new_out,
                        fee_bps,
                    },
                );
                Ok(PoolTouch::ReuseComputedLocally(amount_out))
            }
        }
    }

    /// Records the state observed/derived from a real on-chain quote for a
    /// pool's first touch in this route/size/round. Idempotent per pool_id —
    /// a second call for the same pool_id is a caller bug and is ignored to
    /// avoid clobbering already-mutated state.
    pub fn record_first_touch(
        &mut self,
        pool_id: &str,
        leg_index: usize,
        state: SimulatedPoolState,
    ) {
        self.first_touch_leg
            .entry(pool_id.to_string())
            .or_insert(leg_index);
        self.states.entry(pool_id.to_string()).or_insert(state);
    }
}

/// Marginal/reference rate implied by pool reserves before a swap (constant
/// product only — used for price-impact reporting on the fixture/test path).
pub fn constant_product_reference_rate(reserve_in: U256, reserve_out: U256) -> Option<f64> {
    let ri: f64 = reserve_in.to_string().parse().ok()?;
    let ro: f64 = reserve_out.to_string().parse().ok()?;
    (ri > 0.0).then_some(ro / ri).filter(|r| r.is_finite())
}

/// `(reference_rate/effective_rate - 1) * 10_000`, i.e. how many bps worse
/// the effective execution rate is vs. the pre-trade marginal rate. Positive
/// means degradation (effective rate below reference).
pub fn price_impact_bps(reference_rate: f64, effective_rate: f64) -> Option<i64> {
    if !reference_rate.is_finite() || !effective_rate.is_finite() || reference_rate <= 0.0 {
        return None;
    }
    let ratio = (reference_rate - effective_rate) / reference_rate;
    if !ratio.is_finite() {
        return None;
    }
    Some((ratio * 10_000.0).round() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_curve_is_stableswap() {
        assert_eq!(
            PoolKind::classify("Curve", "CurveStableSwap"),
            PoolKind::CurveStableSwap
        );
    }

    #[test]
    fn classify_uniswap_v3_is_concentrated() {
        assert_eq!(
            PoolKind::classify("UniswapV3", "V3"),
            PoolKind::UniswapV3Concentrated
        );
    }

    #[test]
    fn classify_quickswap_v2_is_constant_product() {
        assert_eq!(
            PoolKind::classify("QuickSwap", "V2"),
            PoolKind::UniswapV2ConstantProduct
        );
    }

    #[test]
    fn only_constant_product_supports_reuse_mutation() {
        assert!(PoolKind::UniswapV2ConstantProduct.supports_exact_reuse_mutation());
        assert!(!PoolKind::UniswapV3Concentrated.supports_exact_reuse_mutation());
        assert!(!PoolKind::CurveStableSwap.supports_exact_reuse_mutation());
    }

    #[test]
    fn constant_product_matches_hand_computed_example() {
        // reserves 1_000_000 / 1_000_000, fee 30bps, amount_in=1000
        // amount_in_with_fee = 1000*9970 = 9_970_000
        // numerator = 9_970_000 * 1_000_000 = 9_970_000_000_000
        // denominator = 1_000_000*10_000 + 9_970_000 = 10_009_970_000
        // amount_out = 9_970_000_000_000 / 10_009_970_000 = 996.006...
        let out = constant_product_amount_out(
            U256::from(1000u64),
            U256::from(1_000_000u64),
            U256::from(1_000_000u64),
            30,
            "pool",
            0,
        )
        .unwrap();
        assert_eq!(out, U256::from(996u64));
    }

    #[test]
    fn first_touch_then_reuse_diverges_from_naive_original_state_reuse() {
        let mut tracker = PoolReuseTracker::new();
        let pool_id = "curve_style_pool";
        let reserve_in = U256::from(1_000_000u64);
        let reserve_out = U256::from(1_000_000u64);
        let fee_bps = 30u32;
        let amount_in = U256::from(100_000u64);

        // Leg 0: first touch. Caller would have fetched a real on-chain
        // quote here; we simulate it via the same formula for the fixture
        // and record the resulting reserve state.
        assert_eq!(
            tracker.touch(pool_id, 0, amount_in).unwrap(),
            PoolTouch::FirstTouch
        );
        let leg0_out =
            constant_product_amount_out(amount_in, reserve_in, reserve_out, fee_bps, pool_id, 0)
                .unwrap();
        let (mutated_in, mutated_out) =
            constant_product_apply(reserve_in, reserve_out, amount_in, leg0_out);
        tracker.record_first_touch(
            pool_id,
            0,
            SimulatedPoolState::ConstantProduct {
                reserve_in: mutated_in,
                reserve_out: mutated_out,
                fee_bps,
            },
        );

        // Leg 2 (same pool reused later in the same route): must be quoted
        // against the MUTATED state, not the original reserves.
        let touch2 = tracker.touch(pool_id, 2, amount_in).unwrap();
        let PoolTouch::ReuseComputedLocally(leg2_out) = touch2 else {
            panic!("expected ReuseComputedLocally, got {touch2:?}");
        };

        // The naive (incorrect) simulation would reuse the ORIGINAL reserves
        // for the second touch too, giving the identical output as leg 0.
        let naive_leg2_out =
            constant_product_amount_out(amount_in, reserve_in, reserve_out, fee_bps, pool_id, 2)
                .unwrap();

        assert_eq!(
            naive_leg2_out, leg0_out,
            "naive model reproduces leg0 output when reusing original reserves"
        );
        assert_ne!(
            leg2_out, naive_leg2_out,
            "correct sequential-state simulation must diverge from the naive same-state reuse"
        );
        assert!(
            leg2_out < leg0_out,
            "second touch against depleted reserve_out must yield strictly less output"
        );
    }

    #[test]
    fn reuse_of_opaque_pool_is_unsupported_not_approximated() {
        let mut tracker = PoolReuseTracker::new();
        let pool_id = "curve_pool_real";
        assert_eq!(
            tracker.touch(pool_id, 0, U256::from(1000u64)).unwrap(),
            PoolTouch::FirstTouch
        );
        tracker.record_first_touch(
            pool_id,
            0,
            SimulatedPoolState::Opaque(PoolKind::CurveStableSwap),
        );
        let err = tracker.touch(pool_id, 2, U256::from(1000u64)).unwrap_err();
        assert_eq!(
            err,
            SimulationError::UnsupportedReusedPool {
                pool_id: pool_id.to_string(),
                leg_index: 2
            }
        );
    }

    #[test]
    fn distinct_pools_do_not_interfere() {
        let mut tracker = PoolReuseTracker::new();
        assert_eq!(
            tracker.touch("pool_a", 0, U256::from(1u64)).unwrap(),
            PoolTouch::FirstTouch
        );
        tracker.record_first_touch(
            "pool_a",
            0,
            SimulatedPoolState::Opaque(PoolKind::UniswapV3Concentrated),
        );
        // pool_b was never touched, so it must still report FirstTouch.
        assert_eq!(
            tracker.touch("pool_b", 1, U256::from(1u64)).unwrap(),
            PoolTouch::FirstTouch
        );
    }

    #[test]
    fn record_first_touch_is_idempotent_does_not_clobber_mutated_state() {
        let mut tracker = PoolReuseTracker::new();
        let pool_id = "pool";
        tracker.touch(pool_id, 0, U256::from(1u64)).unwrap();
        tracker.record_first_touch(
            pool_id,
            0,
            SimulatedPoolState::ConstantProduct {
                reserve_in: U256::from(100u64),
                reserve_out: U256::from(100u64),
                fee_bps: 30,
            },
        );
        // Mutate via a reuse touch.
        let touch = tracker.touch(pool_id, 1, U256::from(10u64)).unwrap();
        assert!(matches!(touch, PoolTouch::ReuseComputedLocally(_)));
        // A stray duplicate record_first_touch call must not reset state
        // back to the pre-mutation reserves.
        tracker.record_first_touch(
            pool_id,
            0,
            SimulatedPoolState::ConstantProduct {
                reserve_in: U256::from(100u64),
                reserve_out: U256::from(100u64),
                fee_bps: 30,
            },
        );
        let touch2 = tracker.touch(pool_id, 2, U256::from(10u64)).unwrap();
        assert!(matches!(touch2, PoolTouch::ReuseComputedLocally(_)));
    }

    #[test]
    fn zero_reserve_is_insufficient_liquidity_not_panic() {
        let err = constant_product_amount_out(
            U256::from(100u64),
            U256::zero(),
            U256::from(100u64),
            30,
            "p",
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SimulationError::InsufficientLiquidity {
                pool_id: "p".to_string(),
                leg_index: 0
            }
        );
    }

    #[test]
    fn tiny_amount_in_can_floor_to_zero_output() {
        let err = constant_product_amount_out(
            U256::from(1u64),
            U256::from(10_000_000_000u64),
            U256::from(1u64),
            30,
            "p",
            0,
        )
        .unwrap_err();
        assert_eq!(
            err,
            SimulationError::ZeroOutput {
                pool_id: "p".to_string(),
                leg_index: 0
            }
        );
    }

    #[test]
    fn price_impact_bps_zero_when_effective_equals_reference() {
        assert_eq!(price_impact_bps(1.0, 1.0), Some(0));
    }

    #[test]
    fn price_impact_bps_positive_when_effective_worse_than_reference() {
        let bps = price_impact_bps(1.0, 0.99).unwrap();
        assert_eq!(bps, 100);
    }

    #[test]
    fn price_impact_bps_none_for_non_finite_inputs() {
        assert_eq!(price_impact_bps(f64::NAN, 1.0), None);
        assert_eq!(price_impact_bps(0.0, 1.0), None);
    }

    #[test]
    fn reference_rate_from_reserves() {
        let r = constant_product_reference_rate(U256::from(2u64), U256::from(4u64)).unwrap();
        assert!((r - 2.0).abs() < 1e-9);
    }

    #[test]
    fn peek_reference_rate_none_before_first_touch() {
        let tracker = PoolReuseTracker::new();
        assert_eq!(tracker.peek_reference_rate("unseen"), None);
    }

    #[test]
    fn peek_reference_rate_reflects_pre_swap_state() {
        let mut tracker = PoolReuseTracker::new();
        let pool_id = "pool";
        tracker.touch(pool_id, 0, U256::from(1u64)).unwrap();
        tracker.record_first_touch(
            pool_id,
            0,
            SimulatedPoolState::ConstantProduct {
                reserve_in: U256::from(1_000_000u64),
                reserve_out: U256::from(2_000_000u64),
                fee_bps: 30,
            },
        );
        let rate = tracker.peek_reference_rate(pool_id).unwrap();
        assert!((rate - 2.0).abs() < 1e-9);
    }

    #[test]
    fn peek_reference_rate_none_for_opaque_pool() {
        let mut tracker = PoolReuseTracker::new();
        tracker.touch("p", 0, U256::from(1u64)).unwrap();
        tracker.record_first_touch(
            "p",
            0,
            SimulatedPoolState::Opaque(PoolKind::CurveStableSwap),
        );
        assert_eq!(tracker.peek_reference_rate("p"), None);
    }
}
