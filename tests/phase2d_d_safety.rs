//! Phase 2D-D safety regression: the fork-bytecode-validation binary must
//! never be able to sign or broadcast a transaction to real Polygon, and
//! its only write-capable RPC target must be a loopback Anvil instance it
//! spawns itself. Enforced via the pure `fork_execution_domain` gate plus
//! static source inspection (no `SignerMiddleware`/`LocalWallet`/private-key
//! handling anywhere in this phase's code).

use flashloan_bot::core::fork_execution_domain::{validate_loopback_endpoint, EndpointRejection};

const BINARY_SOURCE: &str = include_str!("../tools/legacy_diagnostics/phase2d_d_fork_bytecode.rs");
const EXECUTOR_SOURCE: &str = include_str!("../src/core/fork_route_executor.rs");
const DEDUP_SOURCE: &str = include_str!("../src/core/fork_candidate_dedup.rs");
const DOMAIN_SOURCE: &str = include_str!("../src/core/fork_execution_domain.rs");
const TRACE_SOURCE: &str = include_str!("../src/core/fork_trace_validation.rs");

const FORBIDDEN_SYMBOLS: &[&str] = &[
    "LocalWallet",
    "SignerMiddleware",
    "AppMiddleware",
    "PRIVATE_KEY",
    "private_key",
    "Wallet::new",
    "FlashloanExecutor",
    "eth_sendRawTransaction",
];

fn assert_source_clean(name: &str, source: &str) {
    for term in FORBIDDEN_SYMBOLS {
        assert!(
            !source.contains(term),
            "forbidden symbol `{term}` found in {name} — Phase 2D-D must never load a production signer"
        );
    }
}

#[test]
fn phase2d_d_source_never_references_production_signer() {
    assert_source_clean("phase2d_d_fork_bytecode.rs", BINARY_SOURCE);
    assert_source_clean("fork_route_executor.rs", EXECUTOR_SOURCE);
    assert_source_clean("fork_candidate_dedup.rs", DEDUP_SOURCE);
    assert_source_clean("fork_execution_domain.rs", DOMAIN_SOURCE);
    assert_source_clean("fork_trace_validation.rs", TRACE_SOURCE);
}

#[test]
fn phase2d_d_rejects_non_loopback_transaction_endpoint() {
    for url in [
        "https://polygon-rpc.com",
        "https://polygon-mainnet.g.alchemy.com/v2/abc",
        "http://192.168.1.50:8545",
    ] {
        assert!(
            matches!(
                validate_loopback_endpoint(url),
                Err(EndpointRejection::NotLoopback(_))
            ),
            "expected {url} to be rejected as non-loopback"
        );
    }
}

#[test]
fn phase2d_d_accepts_loopback_transaction_endpoint() {
    assert!(validate_loopback_endpoint("http://127.0.0.1:8547").is_ok());
}

#[test]
fn phase2d_d_validates_fork_rpc_before_any_transaction() {
    // The binary must call validate_loopback_endpoint on --fork-rpc before
    // spawning Anvil or sending anything.
    let validate_pos = BINARY_SOURCE
        .find("validate_loopback_endpoint(&cli.fork_rpc)")
        .expect("expected fork_rpc to be validated");
    let spawn_pos = BINARY_SOURCE
        .find("spawn_anvil(&archive_rpc")
        .expect("expected spawn_anvil call");
    assert!(
        validate_pos < spawn_pos,
        "fork_rpc must be validated before Anvil is even spawned"
    );
}

#[test]
fn phase2d_d_never_writes_to_upstream_rpc() {
    // The archive RPC url is read once from the named env var and from then
    // on must only ever be handed to Anvil's own fork config (spawn_anvil /
    // anvil_reset_to_block) — never used to construct a Provider that this
    // process sends transactions through directly.
    assert!(
        !BINARY_SOURCE.contains("Provider::<Http>::try_from(archive_rpc")
            && !BINARY_SOURCE.contains("Provider::<Http>::try_from(&archive_rpc"),
        "archive_rpc must never be dialed directly as a write-capable Provider"
    );
    assert!(
        BINARY_SOURCE.contains("spawn_anvil(&archive_rpc"),
        "archive_rpc should only feed Anvil's fork config"
    );
    assert!(
        !BINARY_SOURCE.contains("send_transaction")
            || BINARY_SOURCE
                .matches("Provider::<Http>::try_from(cli.fork_rpc")
                .count()
                >= 1,
        "the only Provider this process sends transactions through must come from --fork-rpc"
    );
}

#[test]
fn phase2d_d_uses_actual_leg1_output_as_leg2_input() {
    // leg 2 (Curve exchange_underlying / its get_dy_underlying quote) must
    // consume `leg1_actual` (the real balance-delta output of leg 1), never
    // the leg-1 quote or the original nominal input amount. Formatting
    // (single-line vs. multi-line tuple args) is not load-bearing here, so
    // this checks a nearby window of source rather than an exact literal.
    // `rfind`, not `find`: the method name also appears once in the ABI
    // JSON constant near the top of the file — the call site is later.
    fn window_after(source: &str, needle: &str, span: usize) -> String {
        let pos = source
            .rfind(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found in source"));
        source[pos..(pos + span).min(source.len())].to_string()
    }

    let quote_window = window_after(BINARY_SOURCE, "\"get_dy_underlying\",", 200);
    assert!(
        quote_window.contains("leg1_actual"),
        "expected leg2's get_dy_underlying quote to use leg1_actual, got: {quote_window}"
    );

    let exchange_window = window_after(BINARY_SOURCE, "\"exchange_underlying\",", 300);
    assert!(
        exchange_window.contains("leg1_actual") && exchange_window.contains("leg2_min_out"),
        "expected leg2's exchange_underlying call to spend leg1_actual with leg2_min_out, got: {exchange_window}"
    );
}

#[test]
fn phase2d_d_balance_delta_is_the_canonical_leg1_output() {
    // leg1_actual must come from a real balanceOf call, not the router's
    // returned/decoded swap output.
    assert!(BINARY_SOURCE.contains("let leg1_actual = usdt_balance_after_leg1;"));
    assert!(BINARY_SOURCE.contains(r#".method::<_, U256>("balanceOf", executor_account)?"#));
}

#[test]
fn phase2d_d_cli_exposes_no_ambiguous_live_flag() {
    for flag in [
        "--live",
        "--broadcast-mainnet",
        "--real",
        "--send-to-polygon",
    ] {
        assert!(
            !BINARY_SOURCE.contains(flag),
            "CLI must not expose ambiguous flag `{flag}`"
        );
    }
}

#[test]
fn phase2d_d_safety_gates_hardcoded_false() {
    for marker in [
        "PRODUCTION_SIGNER_LOADED=false",
        "PRODUCTION_BROADCASTER_INITIALIZED=false",
        "TRANSACTION_BROADCAST_ALLOWED=false",
        "CYCLES_ECONOMICALLY_TRUSTED=false",
        "LIVE_EXECUTION_AUTHORIZED=false",
        "MAINNET_WRITE_RPC_CALLS=0",
        "MAINNET_TRANSACTIONS_SENT=0",
    ] {
        assert!(
            BINARY_SOURCE.contains(marker),
            "expected gates output to contain `{marker}`"
        );
    }
}

#[test]
fn phase2d_d_archive_rpc_value_is_never_formatted_into_a_log_string() {
    // The env var itself may be referenced (to read it), but its *value*
    // must never be interpolated into an eprintln!/println! call.
    for line in BINARY_SOURCE.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("eprintln!") || trimmed.starts_with("println!") {
            assert!(
                !trimmed.contains("archive_rpc"),
                "archive_rpc must never appear inside a log line: {trimmed}"
            );
        }
    }
}
