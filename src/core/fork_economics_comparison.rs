//! Phase 2D-D vs Phase 2D-C comparison. Pure logic — no RPC.
//!
//! Classifies how a fork-actual value (a real on-chain quote, or a real
//! balance-delta output) compares against a Phase 2D-C predicted value, or
//! how a fork's own pre-swap quote compares against its own post-swap actual
//! balance delta. Every classification is either an exact match, within an
//! explicitly documented per-venue atomic-rounding bound, "economically
//! equivalent" (same conclusion, small relative drift), a material
//! divergence, or — worst case — a sign flip. No global fuzzy tolerance.
#![deny(clippy::arithmetic_side_effects)]

use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum DeltaClassification {
    ExactMatch,
    WithinDocumentedAtomicBound,
    EconomicallyEquivalent,
    MaterialDivergence,
    SignFlip,
}

impl DeltaClassification {
    pub fn label(self) -> &'static str {
        match self {
            Self::ExactMatch => "EXACT_MATCH",
            Self::WithinDocumentedAtomicBound => "WITHIN_DOCUMENTED_ATOMIC_BOUND",
            Self::EconomicallyEquivalent => "ECONOMICALLY_EQUIVALENT",
            Self::MaterialDivergence => "MATERIAL_DIVERGENCE",
            Self::SignFlip => "SIGN_FLIP",
        }
    }
}

/// `predicted`/`actual` are signed atomic-unit values (e.g. PnL, or a
/// quote-vs-actual amount where negative doesn't apply and both are always
/// >= 0 — callers pass 0 as the "sign" baseline for pure quote comparisons).
///
/// `atomic_bound`: a venue-documented rounding allowance (e.g. 1 atomic unit
/// per hop from integer division truncation) — NOT a percentage.
/// `equivalence_bps`: a looser, explicitly-disclosed relative threshold for
/// "same conclusion, different magnitude" — e.g. 50 bps. Must be passed by
/// the caller; never hardcoded here as a hidden default.
pub fn classify_delta(
    predicted: i128,
    actual: i128,
    atomic_bound: u128,
    equivalence_bps: u32,
) -> DeltaClassification {
    let predicted_sign = predicted.signum();
    let actual_sign = actual.signum();
    if predicted_sign != 0 && actual_sign != 0 && predicted_sign != actual_sign {
        return DeltaClassification::SignFlip;
    }
    // predicted>0, actual==0 (or vice versa) is also a meaningful flip from
    // "profitable" to "break-even/not profitable" — treat >0 vs <=0 as the
    // economically relevant sign boundary.
    if (predicted > 0) != (actual > 0) {
        return DeltaClassification::SignFlip;
    }

    let delta = predicted.abs_diff(actual);
    if delta == 0 {
        return DeltaClassification::ExactMatch;
    }
    if delta <= atomic_bound {
        return DeltaClassification::WithinDocumentedAtomicBound;
    }
    let base = predicted.unsigned_abs().max(1);
    let delta_bps = delta
        .saturating_mul(10_000)
        .checked_div(base)
        .unwrap_or(u128::MAX);
    if delta_bps <= equivalence_bps as u128 {
        return DeltaClassification::EconomicallyEquivalent;
    }
    DeltaClassification::MaterialDivergence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_match_when_identical() {
        assert_eq!(
            classify_delta(1000, 1000, 2, 50),
            DeltaClassification::ExactMatch
        );
    }

    #[test]
    fn within_atomic_bound_for_tiny_rounding_delta() {
        assert_eq!(
            classify_delta(1000, 1001, 2, 50),
            DeltaClassification::WithinDocumentedAtomicBound
        );
    }

    #[test]
    fn economically_equivalent_for_small_relative_delta_beyond_atomic_bound() {
        // delta=10 on base=1000 -> 100 bps... use base large enough for <=50bps
        assert_eq!(
            classify_delta(100_000, 100_400, 2, 50),
            DeltaClassification::EconomicallyEquivalent
        );
    }

    #[test]
    fn material_divergence_for_large_relative_delta() {
        assert_eq!(
            classify_delta(100_000, 90_000, 2, 50),
            DeltaClassification::MaterialDivergence
        );
    }

    #[test]
    fn sign_flip_when_predicted_positive_actual_negative() {
        assert_eq!(
            classify_delta(1000, -1000, 2, 50),
            DeltaClassification::SignFlip
        );
    }

    #[test]
    fn sign_flip_when_predicted_positive_actual_zero() {
        assert_eq!(
            classify_delta(1000, 0, 2, 50),
            DeltaClassification::SignFlip
        );
    }

    #[test]
    fn sign_flip_takes_priority_over_small_delta() {
        // delta is tiny (1) but crosses the profitability boundary.
        assert_eq!(classify_delta(1, -1, 5, 50), DeltaClassification::SignFlip);
    }

    #[test]
    fn both_negative_is_not_a_sign_flip() {
        let c = classify_delta(-1000, -1010, 2, 50);
        assert_ne!(c, DeltaClassification::SignFlip);
    }

    #[test]
    fn both_zero_is_exact_match_not_sign_flip() {
        assert_eq!(classify_delta(0, 0, 2, 50), DeltaClassification::ExactMatch);
    }
}
