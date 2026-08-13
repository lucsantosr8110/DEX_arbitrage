//! Proves `CanonicalDiscoveryService::discover_at` runs the real operational
//! pipeline end to end: quote -> edge -> graph -> structural route ->
//! sequential (chained) re-quote -> `CanonicalExecutionContext` ->
//! materialization -> pure route economics -> `CanonicalDiscoveryResult`.
//!
//! Like `tests/phase2d_d_fork_integration.rs`, this spawns a *real* local
//! Anvil fork of Polygon at a fixed historical anchor block via a real
//! archive RPC endpoint — the pinned block makes on-chain state (and so the
//! quotes/edges/routes derived from it) deterministic across runs. There is
//! no meaningful way to exercise the real `canonical_adapters` RPC calls
//! with a mock; this fails loudly (not silently skipped) if `anvil` or
//! `POLYGON_ARCHIVE_RPC_URL` is missing, matching this phase's fail-closed
//! philosophy.

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::H256;
use flashloan_bot::config::Config;
use flashloan_bot::core::canonical_discovery::{
    CanonicalDiscoveryConfig, CanonicalDiscoveryProfile, CanonicalDiscoveryService,
};
use flashloan_bot::core::execution_profile::{ExecutionProfile, MAIN_PENDING_DRY_RUN_PROFILE};
use flashloan_bot::core::fork_route_executor::{spawn_anvil, wait_for_anvil_ready};
use flashloan_bot::core::phase2d_anchor::AnchorBlock;
use serial_test::serial;
use std::path::PathBuf;
use std::time::Duration;

const ANCHOR_BLOCK: u64 = 91_149_850;
const CHAIN_ID: u64 = 137;

fn archive_rpc() -> String {
    std::env::var("POLYGON_ARCHIVE_RPC_URL")
        .expect("POLYGON_ARCHIVE_RPC_URL must be set to run this operational discovery test")
}

struct Guard(std::process::Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
#[serial(polygon_fork)]
async fn discover_at_runs_the_real_operational_pipeline() {
    let archive = archive_rpc();
    let port = 8647u16;
    let child = spawn_anvil(&archive, ANCHOR_BLOCK, CHAIN_ID, port).expect("anvil spawn failed");
    let _guard = Guard(child);

    let provider = std::sync::Arc::new(
        Provider::<Http>::try_from(format!("http://127.0.0.1:{port}"))
            .expect("provider construction failed"),
    );
    wait_for_anvil_ready(&provider, Duration::from_secs(180))
        .await
        .expect("anvil never became ready");

    let block = provider
        .get_block(ANCHOR_BLOCK)
        .await
        .expect("get_block RPC call failed")
        .expect("anchor block missing from fork");
    let hash = block.hash.expect("anchor block must have a real hash");
    assert_ne!(hash, H256::zero(), "anchor block hash must be real");

    let anchor = AnchorBlock {
        number: ANCHOR_BLOCK,
        hash,
        selected_from_head: ANCHOR_BLOCK,
        confirmation_lag: 0,
    };

    let cfg = Config::from_file(PathBuf::from("config/config.toml"))
        .expect("config/config.toml must load")
        .lock()
        .await
        .clone();
    let discovery_config = CanonicalDiscoveryConfig::from_config(
        &cfg,
        CanonicalDiscoveryProfile::Base,
        ExecutionProfile {
            chain_id: CHAIN_ID,
            profile_label: MAIN_PENDING_DRY_RUN_PROFILE.into(),
        },
    )
    .expect("base profile must resolve a real token/venue universe");
    let service = CanonicalDiscoveryService::new(provider, CHAIN_ID, discovery_config);
    let result = service
        .discover_at(anchor)
        .await
        .expect("discover_at must run the real pipeline, not error out");

    // quote -> edge
    assert!(
        result.stats.quotes_attempted > 0,
        "no quotes were attempted"
    );
    assert!(
        result.stats.quotes_succeeded > 0,
        "no quotes succeeded against the pinned fork"
    );
    // edge -> graph
    assert!(result.stats.edges_created > 0, "no executable edges built");
    // graph -> route (structural cycle discovery)
    assert!(
        result.stats.routes_discovered > 0,
        "no structural routes discovered from the executable edge graph"
    );
    // route -> sequential amounts -> context -> materialization
    assert!(
        result.stats.routes_materialized > 0,
        "no routes were materialized into ExecutableRoutePlan"
    );
    // materialization -> integer economics
    assert!(
        result.stats.economics_evaluated > 0,
        "no routes reached pure route economics evaluation"
    );

    // Every materialized route is real typed data, never a placeholder.
    for plan in &result.executable_routes {
        assert!(!plan.start_token.is_zero());
        assert!(!plan.legs.is_empty());
        for leg in &plan.legs {
            assert!(!leg.token_in.is_zero());
            assert!(!leg.token_out.is_zero());
            assert!(!leg.pool.is_zero());
        }
    }

    // Every evidence entry is tied to the same pinned anchor and carries a
    // real (non-zero) execution context hash — never a placeholder result.
    for evidence in &result.round_evidence {
        assert_eq!(evidence.anchor.number, ANCHOR_BLOCK);
        assert_ne!(evidence.context_hash, H256::zero());
    }
}
