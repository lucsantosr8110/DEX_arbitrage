//! Deterministic E1-D/E smoke coverage. Online Polygon-fork cases are
//! intentionally feature/environment gated; no archive endpoint is embedded.

use ethers::providers::{Http, Provider};
use ethers::types::{Address, U256};
use flashloan_bot::core::{
    executable_readonly::{AnvilExecutableReadOnlyVerifier, ExecutableReadOnlyError},
    fork_execution_domain::validate_loopback_endpoint,
    fork_preflight::{
        actual_output, classify_trace, propagate_output, validate_preflight_trace, PreflightStatus,
    },
};
use serde_json::json;
use std::{collections::HashSet, sync::Arc};

#[test]
fn external_write_endpoint_is_rejected() {
    assert!(validate_loopback_endpoint("https://polygon-rpc.com").is_err());
}

#[test]
fn preflight_uses_balance_delta() {
    assert_eq!(
        actual_output(U256::from(100), U256::from(140)).unwrap(),
        U256::from(40)
    );
}

#[test]
fn preflight_propagates_actual_output() {
    assert!(propagate_output(U256::from(40), U256::from(40)).is_ok());
}

#[test]
fn trace_rejects_unexpected_call() {
    let target = Address::from_low_u64_be(44);
    let trace =
        json!({"type":"CALL","to":format!("{target:#x}"),"value":"0x0","input":"0x12345678"});
    let anomalies = validate_preflight_trace(&trace, &HashSet::new(), &HashSet::new());
    assert_eq!(classify_trace(&anomalies), PreflightStatus::UnexpectedCall);
}

#[test]
fn readonly_rejects_non_loopback_endpoint() {
    let provider = Arc::new(Provider::<Http>::try_from("http://127.0.0.1:8545").unwrap());
    assert!(matches!(
        AnvilExecutableReadOnlyVerifier::new("https://example.invalid".into(), provider),
        Err(ExecutableReadOnlyError::NonLoopbackForkEndpoint)
    ));
}
