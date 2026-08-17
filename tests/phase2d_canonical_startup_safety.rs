//! Phase 2D — canonical primary wiring: signer-free startup guarantees.
//!
//! Static, source-level regression guards (matching the
//! `tests/phase2d_c2_safety.rs` pattern): the canonical engine-resolution
//! branch and everything reachable from `run_canonical_mode` must never
//! construct a wallet, signer middleware, broadcaster, or legacy
//! `ArbitrageClient` — before or after this phase's changes.

const MAIN_SRC: &str = include_str!("../src/main.rs");
const CANONICAL_SIMULATION_SRC: &str = include_str!("../src/core/canonical_simulation.rs");
const CANONICAL_DISCOVERY_SRC: &str = include_str!("../src/core/canonical_discovery.rs");

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

#[test]
fn canonical_engine_is_resolved_before_private_key() {
    let decision_idx = MAIN_SRC
        .find("DiscoveryEngine::resolve(&cfg_unlocked.c2b_shadow.discovery_engine)")
        .expect("engine resolution must exist in main()");
    let private_key_idx = MAIN_SRC
        .find("let private_key = std::env::var(\"PRIVATE_KEY\")")
        .expect("legacy PRIVATE_KEY bootstrap must exist in main()");
    assert!(
        decision_idx < private_key_idx,
        "DISCOVERY_ENGINE must be resolved, and the canonical branch returned from, \
         strictly before PRIVATE_KEY is ever read"
    );
}

#[test]
fn canonical_startup_does_not_construct_signer() {
    let src = canonical_mode_source();
    for forbidden in [
        "LocalWallet",
        "SignerMiddleware",
        "with_signer",
        "PRIVATE_KEY",
    ] {
        assert!(
            !src.contains(forbidden),
            "run_canonical_mode must never reference `{forbidden}`"
        );
    }
}

#[test]
fn canonical_startup_does_not_construct_broadcaster() {
    let src = canonical_mode_source();
    for forbidden in [
        "Broadcaster",
        "bundle_sender",
        "BundleSender",
        "send_atomic_flashloan",
    ] {
        assert!(
            !src.contains(forbidden),
            "run_canonical_mode must never reference `{forbidden}`"
        );
    }
}

#[test]
fn canonical_startup_does_not_construct_legacy_arbitrage_client() {
    let src = canonical_mode_source();
    assert!(!src.contains("ArbitrageClient::new"));
    assert!(!src.contains("ArbitrageClient {"));
}

#[test]
fn canonical_discovery_uses_read_only_provider() {
    let start = MAIN_SRC
        .find("Decide engine before touching PRIVATE_KEY")
        .expect("engine-decision comment must exist");
    let branch = &MAIN_SRC[start..start + 900.min(MAIN_SRC.len() - start)];
    assert!(
        branch.contains("Provider::<Http>::try_from"),
        "canonical branch must build a bare read-only Provider<Http>"
    );
    assert!(
        !branch.contains("RpcProvider::connect_http_with_fallback"),
        "canonical branch must never use the signer-requiring legacy connector"
    );
}

#[test]
fn canonical_simulation_does_not_require_signer() {
    // "SignerMiddleware" itself appears only in the module's own doc
    // comment explaining what it deliberately does *not* accept — check
    // for actual usage (a type reference), not the bare word.
    for forbidden in [
        "SignerMiddleware<",
        "LocalWallet",
        "PRIVATE_KEY",
        "private_key",
    ] {
        assert!(
            !CANONICAL_SIMULATION_SRC.contains(forbidden),
            "canonical_simulation.rs must never reference `{forbidden}`"
        );
    }
    assert!(CANONICAL_SIMULATION_SRC.contains("pub fn new(provider: Arc<M>, from: Address)"));
    assert!(CANONICAL_SIMULATION_SRC
        .contains("pub const fn signer_allowed(self) -> bool {\n        false\n    }"));
}

#[test]
fn canonical_discovery_service_has_no_signer_dependency() {
    for forbidden in ["SignerMiddleware", "LocalWallet", "PRIVATE_KEY"] {
        assert!(
            !CANONICAL_DISCOVERY_SRC.contains(forbidden),
            "canonical_discovery.rs must never reference `{forbidden}`"
        );
    }
}
