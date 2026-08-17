//! E1-D/E preflight accounting primitives. RPC orchestration remains in the
//! existing Anvil executor; this module makes the safety-critical decisions
//! explicit and testable.

use crate::core::{
    fork_balance_accounting::{signed_delta, BalanceError},
    fork_trace_validation::{validate_trace, AnomalyKind, TraceAnomaly},
};
use ethers::types::{Address, H256, U256};
use serde_json::Value;
use std::collections::HashSet;
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreflightStatus {
    Pass,
    Revert,
    InvalidCalldata,
    BalanceMismatch,
    UnexpectedCall,
    Unsupported,
    EnvironmentFailure,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreflightLegResult {
    pub leg_index: usize,
    pub target: Address,
    pub selector: [u8; 4],
    pub receipt_status: Option<u64>,
    pub gas_used: Option<u64>,
    pub balance_before: U256,
    pub balance_after: U256,
    pub actual_amount_out: U256,
    pub status: PreflightStatus,
    pub revert_code: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputPropagation {
    pub previous_actual_output: U256,
    pub next_amount_in: U256,
    pub matches: bool,
}

#[derive(Debug, Error)]
pub enum PreflightAccountingError {
    #[error("BALANCE_DELTA_FAILED: {0}")]
    BalanceDelta(#[from] BalanceError),
    #[error("OUTPUT_PROPAGATION_MISMATCH expected={expected} actual={actual}")]
    OutputPropagationMismatch { expected: U256, actual: U256 },
}

pub fn actual_output(
    balance_before: U256,
    balance_after: U256,
) -> Result<U256, PreflightAccountingError> {
    let delta = signed_delta(balance_before, balance_after)?;
    if delta < 0 {
        return Err(PreflightAccountingError::OutputPropagationMismatch {
            expected: U256::zero(),
            actual: balance_after,
        });
    }
    Ok(U256::from(delta as u128))
}

pub fn propagate_output(
    previous_actual_output: U256,
    next_amount_in: U256,
) -> Result<OutputPropagation, PreflightAccountingError> {
    let matches = previous_actual_output == next_amount_in;
    if !matches {
        return Err(PreflightAccountingError::OutputPropagationMismatch {
            expected: previous_actual_output,
            actual: next_amount_in,
        });
    }
    Ok(OutputPropagation {
        previous_actual_output,
        next_amount_in,
        matches,
    })
}

pub fn validate_preflight_trace(
    trace: &Value,
    allowlist: &HashSet<Address>,
    callbacks: &HashSet<[u8; 4]>,
) -> Vec<TraceAnomaly> {
    validate_trace(trace, allowlist, callbacks)
}

pub fn classify_trace(anomalies: &[TraceAnomaly]) -> PreflightStatus {
    if anomalies.iter().any(|a| {
        matches!(
            a.kind,
            AnomalyKind::UnexpectedCallTarget
                | AnomalyKind::UnexpectedDelegateTarget
                | AnomalyKind::UnexpectedNativeValueTransfer
                | AnomalyKind::SelfDestruct
        )
    }) {
        PreflightStatus::UnexpectedCall
    } else if anomalies
        .iter()
        .any(|a| a.kind == AnomalyKind::InternalRevert)
    {
        PreflightStatus::Revert
    } else {
        PreflightStatus::Pass
    }
}

pub fn trace_hash(trace: &Value) -> H256 {
    H256::from(ethers::utils::keccak256(
        serde_json::to_vec(trace).unwrap_or_default(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn preflight_uses_balance_delta() {
        assert_eq!(
            actual_output(U256::from(100), U256::from(145)).unwrap(),
            U256::from(45)
        );
    }
    #[test]
    fn preflight_propagates_actual_output() {
        assert!(
            propagate_output(U256::from(45), U256::from(45))
                .unwrap()
                .matches
        );
        assert!(propagate_output(U256::from(45), U256::from(44)).is_err());
    }
    #[test]
    fn trace_rejects_unexpected_call() {
        let evil = Address::from_low_u64_be(9);
        let t = json!({"type":"CALL","to":format!("{evil:#x}"),"value":"0x0","input":"0x12345678"});
        let a = validate_preflight_trace(&t, &HashSet::new(), &HashSet::new());
        assert_eq!(classify_trace(&a), PreflightStatus::UnexpectedCall);
    }
    #[test]
    fn trace_hash_is_deterministic() {
        let t = json!({"type":"CALL"});
        assert_eq!(trace_hash(&t), trace_hash(&t));
    }
}
