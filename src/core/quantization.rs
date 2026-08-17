//! Conservative human↔atomic unit conversion for Phase 2D-C.
//!
//! `f64` is never the canonical value for an on-chain quantity — it is only
//! ever an ingress/egress boundary. Conversion into atomic (`U256`) units
//! always floors (never rounds a quantity we are about to spend/receive up in
//! our own favor); overflow and non-finite/negative inputs are explicit
//! errors, never a silent clamp.
#![deny(clippy::arithmetic_side_effects)]

use ethers::types::U256;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum QuantizationError {
    #[error("QUANTIZATION_NON_FINITE_OR_NEGATIVE: {0}")]
    NonFiniteOrNegative(String),
    #[error("QUANTIZATION_DECIMALS_OUT_OF_RANGE: decimals={0}")]
    DecimalsOutOfRange(u8),
    #[error("QUANTIZATION_OVERFLOW: {0}")]
    Overflow(String),
}

/// Human-readable amount → atomic (`10^decimals`) units. Floors — never
/// rounds up. `decimals` above 36 is rejected (matches `fixed_usd`'s bound;
/// no real token exceeds it).
pub fn human_to_atomic_floor(human: f64, decimals: u8) -> Result<U256, QuantizationError> {
    if !human.is_finite() || human < 0.0 {
        return Err(QuantizationError::NonFiniteOrNegative(format!(
            "human={human}"
        )));
    }
    if decimals > 36 {
        return Err(QuantizationError::DecimalsOutOfRange(decimals));
    }
    if human == 0.0 {
        return Ok(U256::zero());
    }
    let scaled = human * 10f64.powi(decimals as i32);
    if !scaled.is_finite() || scaled >= 1.0e38 {
        return Err(QuantizationError::Overflow(format!(
            "human={human} decimals={decimals}"
        )));
    }
    // Floor via string-free integer truncation: f64 -> u128 truncates toward
    // zero, which is floor for non-negative values.
    if scaled >= u128::MAX as f64 {
        return Err(QuantizationError::Overflow(format!(
            "human={human} decimals={decimals} exceeds u128"
        )));
    }
    Ok(U256::from(scaled as u128))
}

/// Atomic units → human `f64`. Telemetry/report only — never feed back into
/// a gate as the canonical value.
pub fn atomic_to_human_display(amount: U256, decimals: u8) -> f64 {
    crate::utils::u256_to_f64(amount, decimals as u32)
}

/// `true` if a strictly-positive human amount floored to zero atomic units —
/// the requested size is entirely below the token's atomic precision.
pub fn is_dust(human: f64, atomic: U256) -> bool {
    human > 0.0 && atomic.is_zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_amount_6_decimals() {
        assert_eq!(
            human_to_atomic_floor(100.0, 6).unwrap(),
            U256::from(100_000_000u64)
        );
    }

    #[test]
    fn floors_fractional_atomic_instead_of_rounding() {
        // 1.0000000000005 @ 12 decimals scales to 1_000_000_000_000.5 atomic
        // units — must floor to 1_000_000_000_000, never round up to ...001.
        let raw = human_to_atomic_floor(1.000_000_000_000_5, 12).unwrap();
        assert_eq!(raw, U256::from(1_000_000_000_000u64));
    }

    #[test]
    fn decimals_0_is_whole_units() {
        assert_eq!(human_to_atomic_floor(42.9, 0).unwrap(), U256::from(42u64));
    }

    #[test]
    fn decimals_8_matches_wbtc_scale() {
        assert_eq!(
            human_to_atomic_floor(0.5, 8).unwrap(),
            U256::from(50_000_000u64)
        );
    }

    #[test]
    fn zero_human_is_zero_atomic() {
        assert_eq!(human_to_atomic_floor(0.0, 18).unwrap(), U256::zero());
    }

    #[test]
    fn dust_below_atomic_precision_floors_to_zero_and_is_flagged() {
        let atomic = human_to_atomic_floor(0.0000001, 6).unwrap();
        assert_eq!(atomic, U256::zero());
        assert!(is_dust(0.0000001, atomic));
    }

    #[test]
    fn non_dust_amount_is_not_flagged() {
        let atomic = human_to_atomic_floor(1.0, 6).unwrap();
        assert!(!is_dust(1.0, atomic));
    }

    #[test]
    fn negative_human_rejected() {
        assert!(matches!(
            human_to_atomic_floor(-1.0, 6),
            Err(QuantizationError::NonFiniteOrNegative(_))
        ));
    }

    #[test]
    fn nan_rejected() {
        assert!(matches!(
            human_to_atomic_floor(f64::NAN, 6),
            Err(QuantizationError::NonFiniteOrNegative(_))
        ));
    }

    #[test]
    fn infinite_rejected() {
        assert!(matches!(
            human_to_atomic_floor(f64::INFINITY, 6),
            Err(QuantizationError::NonFiniteOrNegative(_))
        ));
    }

    #[test]
    fn decimals_over_36_rejected() {
        assert!(matches!(
            human_to_atomic_floor(1.0, 37),
            Err(QuantizationError::DecimalsOutOfRange(37))
        ));
    }

    #[test]
    fn huge_amount_overflow_rejected() {
        assert!(matches!(
            human_to_atomic_floor(1.0e30, 18),
            Err(QuantizationError::Overflow(_))
        ));
    }

    #[test]
    fn round_trip_display_matches_input_within_float_precision() {
        let atomic = human_to_atomic_floor(123.456, 6).unwrap();
        let back = atomic_to_human_display(atomic, 6);
        assert!((back - 123.456).abs() < 1e-6);
    }

    #[test]
    fn different_decimals_same_human_differ_in_atomic_scale() {
        let a6 = human_to_atomic_floor(1.0, 6).unwrap();
        let a18 = human_to_atomic_floor(1.0, 18).unwrap();
        assert_eq!(a6, U256::exp10(6));
        assert_eq!(a18, U256::exp10(18));
        assert!(a18 > a6);
    }

    #[test]
    fn monotonic_in_human_amount() {
        let decimals = 18u8;
        let mut prev = U256::zero();
        for human in [0.0, 1.0, 10.0, 100.0, 1000.0] {
            let atomic = human_to_atomic_floor(human, decimals).unwrap();
            assert!(atomic >= prev);
            prev = atomic;
        }
    }
}
