//! Phase 2D-D balance accounting. Pure logic — no RPC.
//!
//! ERC-20 balance deltas, not router/quote return values, are the canonical
//! source of truth for how much a swap actually moved (spec section 14/20):
//! a router can return a value that doesn't match what the token contract
//! actually transferred (fee-on-transfer tokens, rebasing, non-standard
//! ERC-20 quirks) and the balance is what the next leg — or the wallet at
//! the end of the route — actually has to work with.
#![deny(clippy::arithmetic_side_effects)]

use ethers::types::U256;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum BalanceError {
    #[error("BALANCE_OVERFLOW: {0}")]
    Overflow(String),
}

/// `after - before` as a signed delta. Never panics/wraps on underflow —
/// returns a genuine negative value so a token that *lost* balance (e.g. an
/// unexpected fee-on-transfer deduction) is visible, not clipped to zero.
pub fn signed_delta(before: U256, after: U256) -> Result<i128, BalanceError> {
    let before_i =
        i128::try_from(before.as_u128()).map_err(|_| BalanceError::Overflow(before.to_string()))?;
    let after_i =
        i128::try_from(after.as_u128()).map_err(|_| BalanceError::Overflow(after.to_string()))?;
    after_i
        .checked_sub(before_i)
        .ok_or_else(|| BalanceError::Overflow(format!("{before} -> {after}")))
}

/// Gross PnL of the whole route, in the start token's atomic units, from the
/// wallet's own start-token balance before and after both legs.
pub fn gross_pnl_atomic(initial_balance: U256, final_balance: U256) -> Result<i128, BalanceError> {
    signed_delta(initial_balance, final_balance)
}

/// `gross - gas_cost_start_token_atomic`. `gas_cost` is always non-negative
/// (a real cost), so this only ever subtracts.
pub fn net_pnl_atomic(
    gross_pnl: i128,
    gas_cost_start_token_atomic: U256,
) -> Result<i128, BalanceError> {
    let gas_i = i128::try_from(gas_cost_start_token_atomic.as_u128())
        .map_err(|_| BalanceError::Overflow(gas_cost_start_token_atomic.to_string()))?;
    gross_pnl
        .checked_sub(gas_i)
        .ok_or_else(|| BalanceError::Overflow(format!("{gross_pnl} - {gas_i}")))
}

/// A residual balance on a non-start, non-terminal token (e.g. leftover
/// USDT dust after both legs) that shouldn't exist if the route closed
/// cleanly. `threshold_atomic` lets the caller ignore genuine rounding dust
/// while still catching a material stuck balance.
pub fn is_material_residual(residual_atomic: U256, threshold_atomic: U256) -> bool {
    residual_atomic > threshold_atomic
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_delta_when_balance_increases() {
        assert_eq!(
            signed_delta(U256::from(100u64), U256::from(150u64)).unwrap(),
            50
        );
    }

    #[test]
    fn negative_delta_when_balance_decreases() {
        assert_eq!(
            signed_delta(U256::from(150u64), U256::from(100u64)).unwrap(),
            -50
        );
    }

    #[test]
    fn zero_delta_when_unchanged() {
        assert_eq!(
            signed_delta(U256::from(100u64), U256::from(100u64)).unwrap(),
            0
        );
    }

    #[test]
    fn gross_pnl_matches_signed_delta() {
        assert_eq!(
            gross_pnl_atomic(U256::from(1_000_000u64), U256::from(1_000_500u64)).unwrap(),
            500
        );
    }

    #[test]
    fn net_pnl_subtracts_gas_cost() {
        assert_eq!(net_pnl_atomic(1000, U256::from(300u64)).unwrap(), 700);
    }

    #[test]
    fn net_pnl_can_go_negative_when_gas_exceeds_gross() {
        assert_eq!(net_pnl_atomic(100, U256::from(300u64)).unwrap(), -200);
    }

    #[test]
    fn material_residual_above_threshold_is_flagged() {
        assert!(is_material_residual(U256::from(1000u64), U256::from(10u64)));
    }

    #[test]
    fn residual_at_or_below_threshold_is_not_flagged() {
        assert!(!is_material_residual(U256::from(10u64), U256::from(10u64)));
        assert!(!is_material_residual(U256::from(5u64), U256::from(10u64)));
    }
}
