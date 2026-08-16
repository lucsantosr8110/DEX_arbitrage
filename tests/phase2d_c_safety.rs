//! Phase 2D-C regression: the sequential-simulation campaign must never be
//! able to sign or broadcast a transaction. Enforced two ways:
//!
//! 1. Static source inspection — the campaign binary and its supporting
//!    core modules must never reference a signer, private key, wallet, or
//!    any `eth_send*` RPC method, by construction.
//! 2. Ambiguous execution-flag inspection — the CLI must not expose a
//!    `--execute`/`--send`/`--broadcast` flag per spec section 26.
//!
//! These are compile-time-adjacent guards, not a mock-RPC call recorder: the
//! binary only ever imports `ethers::providers::Provider`, never
//! `SignerMiddleware`/`LocalWallet`/`AppMiddleware`, so there is no code path
//! through which a transaction could be signed even if a bug tried.

const CAMPAIGN_SOURCE: &str = include_str!("../tools/legacy_diagnostics/phase2d_c_sequential_sim.rs");
const ROUTE_ARTIFACT_SOURCE: &str = include_str!("../src/core/route_artifact.rs");
const POOL_STATE_SIM_SOURCE: &str = include_str!("../src/core/pool_state_sim.rs");
const QUANTIZATION_SOURCE: &str = include_str!("../src/core/quantization.rs");
const ECONOMICS_SOURCE: &str = include_str!("../src/core/sequential_route_economics.rs");

const FORBIDDEN_SYMBOLS: &[&str] = &[
    "LocalWallet",
    "SignerMiddleware",
    "AppMiddleware",
    "PRIVATE_KEY",
    "private_key",
    "eth_sendRawTransaction",
    "eth_sendTransaction",
    "send_transaction",
    "sign_transaction",
    "signRawTransaction",
    "Wallet::new",
    "FlashloanExecutor",
    "Broadcaster",
    "broadcaster_initialized = true",
];

fn assert_source_clean(name: &str, source: &str) {
    for term in FORBIDDEN_SYMBOLS {
        assert!(
            !source.contains(term),
            "phase2d_c_never_broadcasts: forbidden symbol `{term}` found in {name} — \
             Phase 2D-C must never be able to sign or broadcast a transaction"
        );
    }
}

#[test]
fn phase2d_c_never_broadcasts_campaign_binary_is_clean() {
    assert_source_clean("src/bin/phase2d_c_sequential_sim.rs", CAMPAIGN_SOURCE);
}

#[test]
fn phase2d_c_never_broadcasts_supporting_core_modules_are_clean() {
    assert_source_clean("src/core/route_artifact.rs", ROUTE_ARTIFACT_SOURCE);
    assert_source_clean("src/core/pool_state_sim.rs", POOL_STATE_SIM_SOURCE);
    assert_source_clean("src/core/quantization.rs", QUANTIZATION_SOURCE);
    assert_source_clean("src/core/sequential_route_economics.rs", ECONOMICS_SOURCE);
}

#[test]
fn phase2d_c_campaign_only_imports_read_only_provider_types() {
    assert!(
        CAMPAIGN_SOURCE.contains("providers::{Middleware, Provider}"),
        "expected the campaign binary to import the plain read-only ethers Provider"
    );
    assert!(
        !CAMPAIGN_SOURCE.contains("use ethers_signers"),
        "campaign binary must not depend on ethers_signers"
    );
}

#[test]
fn phase2d_c_cli_exposes_no_ambiguous_execution_flag() {
    for flag in ["--execute", "--send", "--broadcast"] {
        assert!(
            !CAMPAIGN_SOURCE.contains(flag),
            "CLI must not expose ambiguous flag `{flag}` (spec section 26)"
        );
    }
}

#[test]
fn phase2d_c_safety_gates_hardcoded_false() {
    for marker in [
        "LIVE_TRADING_ENABLED=false",
        "TRANSACTION_BROADCAST_ALLOWED=false",
        "SIGNER_LOADED=false",
        "BROADCASTER_INITIALIZED=false",
        "MAINNET_TRANSACTIONS_SENT=0",
        "CYCLES_ECONOMICALLY_TRUSTED=false",
        "LIVE_EXECUTION_AUTHORIZED=false",
    ] {
        assert!(
            CAMPAIGN_SOURCE.contains(marker),
            "expected campaign binary to emit gate `{marker}` in its gates.txt output"
        );
    }
}
