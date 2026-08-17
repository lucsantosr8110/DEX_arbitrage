//! Phase 2D — canonical primary wiring: authority and scheduler guarantees.
//!
//! `run_canonical_mode` in `src/main.rs` is the sole execution authority
//! when `DISCOVERY_ENGINE=canonical`: `main()` returns into it before ever
//! reaching the legacy `PRIVATE_KEY`/`ArbitrageEngine`/execution bootstrap.
//! These are static, source-level regression guards (matching the
//! `tests/phase2d_c2_safety.rs` pattern) — there is no live instrumentation
//! hook to count calls at runtime, so a call that must never happen is
//! proven absent from the reachable source instead.

use ethers::types::H256;
use flashloan_bot::core::c2b_shadow_service::should_schedule_anchor;
use flashloan_bot::core::phase2d_anchor::AnchorBlock;

const MAIN_SRC: &str = include_str!("../src/main.rs");

/// Isolates exactly the `run_canonical_mode` function body — the only
/// code path reachable once `DISCOVERY_ENGINE=canonical` is resolved. The
/// rest of `main.rs` (legacy bootstrap) is intentionally excluded: it
/// legitimately references `ArbitrageEngine` etc., just never from here.
fn canonical_mode_source() -> &'static str {
    let start = MAIN_SRC
        .find("async fn run_canonical_mode")
        .expect("run_canonical_mode must exist in main.rs");
    let rest = &MAIN_SRC[start..];
    let end = rest
        .find("\n// ============================================================")
        .expect("run_canonical_mode must be followed by a section header");
    &rest[..end]
}

/// Isolates the branch in `main()` that decides which engine gets
/// execution authority (the code immediately around the early return into
/// `run_canonical_mode`).
fn engine_decision_source() -> &'static str {
    let start = MAIN_SRC
        .find("Decide engine before touching PRIVATE_KEY")
        .expect("engine-decision comment must exist in main.rs");
    let rest = &MAIN_SRC[start..];
    let end = rest
        .find("let private_key = std::env::var(\"PRIVATE_KEY\")")
        .expect("engine decision must be followed by the legacy PRIVATE_KEY bootstrap");
    &rest[..end]
}

#[test]
fn canonical_mode_does_not_call_arbitrage_engine() {
    let src = canonical_mode_source();
    assert!(
        !src.contains("ArbitrageEngine"),
        "run_canonical_mode must never reference the legacy ArbitrageEngine"
    );
}

#[test]
fn canonical_mode_does_not_call_legacy_select_opportunities() {
    let src = canonical_mode_source();
    for forbidden in ["select_opportunities", "should_try_next_opp"] {
        assert!(
            !src.contains(forbidden),
            "run_canonical_mode must never call legacy selector `{forbidden}`"
        );
    }
}

#[test]
fn canonical_mode_does_not_execute_legacy_opportunity() {
    let src = canonical_mode_source();
    for forbidden in [
        "execute_opportunity_standalone",
        "ArbitrageClient::new",
        "SignerMiddleware",
    ] {
        assert!(
            !src.contains(forbidden),
            "run_canonical_mode must never construct/execute via `{forbidden}`"
        );
    }
}

#[test]
fn canonical_failure_does_not_fallback_to_legacy() {
    let src = canonical_mode_source();
    // Every failure branch (rejected round, timeout) only logs and lets
    // the `tokio::select!` loop continue to the next tick — neither arm
    // returns, breaks into a different code path, or calls into anything
    // legacy-named.
    assert!(src.contains("no legacy fallback"));
    assert!(!src.contains("run_legacy"));
    assert!(!src.contains("fallback_to_legacy"));
}

#[test]
fn only_one_engine_has_execution_authority() {
    let src = engine_decision_source();
    assert!(
        src.contains("return run_canonical_mode("),
        "canonical engine resolution must return straight into run_canonical_mode, \
         never fall through into the legacy bootstrap below it"
    );
}

#[test]
fn canonical_scheduler_max_one_round_in_flight() {
    // No per-round `tokio::spawn`: the single `tokio::select!` tick arm
    // awaits `discover_at` directly, so a second tick physically cannot
    // start a new round before the in-flight one has returned.
    let src = canonical_mode_source();
    assert!(!src.contains("tokio::spawn"));
    assert!(src.contains("service.discover_at(anchor.clone())"));
}

#[test]
fn canonical_timeout_does_not_stop_main_loop() {
    let src = canonical_mode_source();
    assert!(src.contains("Err(_) => warn!(\"canonical round timed out; no legacy fallback\"),"));
    // The timeout arm must not `return`/`break`/`?` out of the loop.
    let timeout_arm_idx = src
        .find("Err(_) => warn!(\"canonical round timed out")
        .unwrap();
    let arm = &src[timeout_arm_idx..(timeout_arm_idx + 80).min(src.len())];
    assert!(!arm.contains("return"));
    assert!(!arm.contains("break"));
}

#[test]
fn canonical_shutdown_cancels_worker() {
    let src = canonical_mode_source();
    assert!(src.contains("shutdown_rx.recv()"));
    assert!(src.contains("CANONICAL_SHUTDOWN_CANCELS_WORKER=true"));
}

// ============================================================
// Scheduler semantics (pure function, exercised directly)
// ============================================================

fn anchor(number: u64) -> AnchorBlock {
    AnchorBlock {
        number,
        hash: H256::from_low_u64_be(number),
        selected_from_head: number,
        confirmation_lag: 0,
    }
}

#[test]
fn canonical_scheduler_uses_block_distance() {
    // A block-distance check (`>=`), not a modulo — e.g. every_n_blocks=32
    // schedules at 100 then the next real anchor at >=132, never "any
    // multiple of 32" (100 is not itself a multiple of 32).
    assert!(should_schedule_anchor(None, &anchor(100), 32, false));
    assert!(!should_schedule_anchor(Some(100), &anchor(120), 32, false));
    assert!(!should_schedule_anchor(Some(100), &anchor(131), 32, false));
    assert!(should_schedule_anchor(Some(100), &anchor(132), 32, false));
}

#[test]
fn canonical_scheduler_does_not_repeat_anchor() {
    assert!(!should_schedule_anchor(Some(100), &anchor(100), 32, false));
    assert!(!should_schedule_anchor(Some(100), &anchor(99), 32, false));
}

#[test]
fn canonical_scheduler_queue_capacity_is_one() {
    // There is no pending-anchor queue/channel in the canonical loop at
    // all — each tick either schedules the current head or is skipped;
    // nothing is buffered for a later tick to drain.
    let src = canonical_mode_source();
    assert!(!src.contains("mpsc::channel"));
    assert!(!src.contains("VecDeque"));
}
