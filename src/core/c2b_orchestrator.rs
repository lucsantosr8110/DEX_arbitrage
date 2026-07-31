//! E1-F2 orchestration contract.
//!
//! This module owns the ordering invariant for executable discovery.  The
//! binary supplies concrete adapters (RPC/fork); tests use a recording adapter
//! to prove every stage is invoked and that no stage can run after rejection.

use ethers::types::U256;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrchestratorEvidence {
    pub structural_cycle_key: String,
    pub economic_positive: bool,
    pub builder_called: bool,
    pub readonly_pass: bool,
    pub preflight_pass: bool,
    pub balance_delta: U256,
    pub output_propagated: bool,
    pub trace_validated: bool,
    pub rejected_registry_hit: bool,
    pub placeholder_evidence: bool,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum OrchestratorError {
    #[error("REJECTED_REGISTRY_HIT")]
    RejectedRegistryHit,
    #[error("ORCHESTRATOR_STAGE_FAILED: {0}")]
    Stage(&'static str),
    #[error("PLACEHOLDER_EVIDENCE")]
    PlaceholderEvidence,
}

/// Concrete adapters implement this trait.  Each method must perform the
/// real stage and return false/error on unsupported or simulated evidence.
pub trait OrchestratorStages {
    fn stateful_economics(&mut self, key: &str) -> Result<bool, OrchestratorError>;
    fn build_executable_call(
        &mut self,
        key: &str,
        amount_in: U256,
    ) -> Result<U256, OrchestratorError>;
    fn readonly_eth_call(&mut self, key: &str, amount_in: U256) -> Result<U256, OrchestratorError>;
    fn anvil_preflight(
        &mut self,
        key: &str,
        amount_in: U256,
    ) -> Result<(U256, bool), OrchestratorError>;
    fn validate_trace(&mut self, key: &str) -> Result<bool, OrchestratorError>;
}

/// Executes one physical route in the only permitted order.  Rejected routes
/// return before economics, calldata, eth_call, or preflight.
pub fn execute_route<S: OrchestratorStages>(
    stages: &mut S,
    structural_cycle_key: &str,
    rejected: bool,
    amount_in: U256,
) -> Result<OrchestratorEvidence, OrchestratorError> {
    if rejected {
        return Err(OrchestratorError::RejectedRegistryHit);
    }
    let economic_positive = stages.stateful_economics(structural_cycle_key)?;
    if !economic_positive {
        return Err(OrchestratorError::Stage("stateful_economics"));
    }
    let built_amount = stages.build_executable_call(structural_cycle_key, amount_in)?;
    if built_amount != amount_in || built_amount.is_zero() {
        return Err(OrchestratorError::PlaceholderEvidence);
    }
    let readonly_amount = stages.readonly_eth_call(structural_cycle_key, built_amount)?;
    if readonly_amount.is_zero() {
        return Err(OrchestratorError::Stage("readonly_eth_call"));
    }
    let (balance_delta, propagated) =
        stages.anvil_preflight(structural_cycle_key, readonly_amount)?;
    if balance_delta.is_zero() || !propagated {
        return Err(OrchestratorError::Stage("balance_delta_or_propagation"));
    }
    let trace_validated = stages.validate_trace(structural_cycle_key)?;
    if !trace_validated {
        return Err(OrchestratorError::Stage("trace_validation"));
    }
    Ok(OrchestratorEvidence {
        structural_cycle_key: structural_cycle_key.into(),
        economic_positive,
        builder_called: true,
        readonly_pass: true,
        preflight_pass: true,
        balance_delta,
        output_propagated: propagated,
        trace_validated,
        rejected_registry_hit: false,
        placeholder_evidence: false,
    })
}
