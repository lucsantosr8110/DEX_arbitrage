use ethers::types::U256;
use flashloan_bot::core::c2b_orchestrator::{execute_route, OrchestratorError, OrchestratorStages};

#[derive(Default)]
struct Recording {
    calls: Vec<&'static str>,
    amount: U256,
}
impl OrchestratorStages for Recording {
    fn stateful_economics(&mut self, _: &str) -> Result<bool, OrchestratorError> {
        self.calls.push("economics");
        Ok(true)
    }
    fn build_executable_call(&mut self, _: &str, amount: U256) -> Result<U256, OrchestratorError> {
        self.calls.push("builder");
        self.amount = amount;
        Ok(amount)
    }
    fn readonly_eth_call(&mut self, _: &str, amount: U256) -> Result<U256, OrchestratorError> {
        self.calls.push("readonly");
        Ok(amount)
    }
    fn anvil_preflight(
        &mut self,
        _: &str,
        amount: U256,
    ) -> Result<(U256, bool), OrchestratorError> {
        self.calls.push("preflight");
        Ok((amount + U256::from(7), true))
    }
    fn validate_trace(&mut self, _: &str) -> Result<bool, OrchestratorError> {
        self.calls.push("trace");
        Ok(true)
    }
}

#[test]
fn orchestrator_invokes_all_real_stages_in_order() {
    let mut r = Recording::default();
    let evidence = execute_route(&mut r, "physical", false, U256::from(100)).unwrap();
    assert_eq!(
        r.calls,
        ["economics", "builder", "readonly", "preflight", "trace"]
    );
    assert!(evidence.economic_positive && evidence.builder_called && evidence.readonly_pass);
    assert!(evidence.preflight_pass && evidence.output_propagated && evidence.trace_validated);
    assert_eq!(evidence.balance_delta, U256::from(107));
    assert!(!evidence.placeholder_evidence);
}

#[test]
fn orchestrator_applies_rejected_registry_first() {
    let mut r = Recording::default();
    assert_eq!(
        execute_route(&mut r, "rejected", true, U256::from(1)),
        Err(OrchestratorError::RejectedRegistryHit)
    );
    assert!(r.calls.is_empty());
}

#[test]
fn orchestrator_never_uses_placeholder_evidence() {
    let mut r = Recording::default();
    let evidence = execute_route(&mut r, "physical", false, U256::from(1)).unwrap();
    assert!(!evidence.placeholder_evidence);
    assert!(!evidence.balance_delta.is_zero());
}

fn run() -> (
    Recording,
    flashloan_bot::core::c2b_orchestrator::OrchestratorEvidence,
) {
    let mut r = Recording::default();
    let e = execute_route(&mut r, "physical", false, U256::from(10)).unwrap();
    (r, e)
}

#[test]
fn orchestrator_invokes_fresh_economics() {
    assert_eq!(run().0.calls[0], "economics");
}
#[test]
fn orchestrator_builds_executable_calls() {
    assert!(run().0.calls.contains(&"builder"));
}
#[test]
fn orchestrator_executes_readonly_verification() {
    assert!(run().0.calls.contains(&"readonly"));
}
#[test]
fn orchestrator_executes_anvil_preflight() {
    assert!(run().0.calls.contains(&"preflight"));
}
#[test]
fn orchestrator_propagates_balance_delta() {
    assert!(run().1.output_propagated && !run().1.balance_delta.is_zero());
}
#[test]
fn orchestrator_validates_traces() {
    assert!(run().1.trace_validated);
}
#[test]
fn orchestrator_requires_three_distinct_rounds() {
    use flashloan_bot::core::fresh_discovery_gate::{distinct_fresh_anchors, RoundEvidence};
    let e = |b| RoundEvidence {
        anchor_block: b,
        quote_only: false,
        economically_positive: true,
        read_only_pass: true,
        preflight_pass: true,
        preflight_reverted: false,
    };
    assert!(distinct_fresh_anchors(&[e(1), e(2), e(3)], &[]));
    assert!(!distinct_fresh_anchors(&[e(1), e(1), e(3)], &[]));
}
