//! Diagnostic-only, single-round harness for the canonical discovery
//! pipeline. Mirrors `run_canonical_mode`'s provider/config construction in
//! `src/main.rs` exactly, minus the TUI/telegram/history/API-server/loop --
//! this calls `CanonicalDiscoveryService::discover_at` exactly once against
//! the real configured RPC and exits.
//!
//! Structurally read-only: `discover_at` accepts no signer and no
//! broadcaster (see its own doc comment in `canonical_discovery.rs`), so
//! there is no code path here that can construct a wallet, sign, or send a
//! transaction. No Anvil fork is spawned -- this reads live chain state
//! directly via `eth_call`/`eth_getBlockByNumber`/`eth_getCode`, nothing
//! that mutates state.
use ethers::providers::{Middleware, Provider};
use flashloan_bot::config::Config;
use flashloan_bot::core::canonical_discovery::{
    CanonicalDiscoveryConfig, CanonicalDiscoveryProfile, CanonicalDiscoveryService,
};
use flashloan_bot::core::execution_profile::{ExecutionProfile, MAIN_PENDING_DRY_RUN_PROFILE};
use flashloan_bot::core::phase2d_anchor::AnchorBlock;
use flashloan_bot::infra::rotating_http_client::RotatingHttpClient;
use flashloan_bot::infra::rpc_provider::is_usable_endpoint;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();

    let env_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    if dotenvy::from_path(&env_path).is_err() {
        let _ = dotenvy::dotenv();
    }

    let config_path = std::env::var("CONFIG_FILE").unwrap_or_else(|_| "config/config.toml".into());
    let config = Config::from_file(PathBuf::from(config_path))?;
    let cfg = {
        let lock = config.lock().await;
        Arc::new(lock.clone())
    };

    let rpc_endpoints: Vec<String> = match std::env::var("BOT_RPC_ENDPOINTS") {
        Ok(raw) if !raw.trim().is_empty() => raw
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect(),
        _ => cfg.network.rpc_endpoints.clone().unwrap_or_default(),
    };
    let usable: Vec<String> = rpc_endpoints
        .into_iter()
        .filter(|e| is_usable_endpoint(e))
        .collect();
    anyhow::ensure!(!usable.is_empty(), "CANONICAL_ONE_ROUND_NO_USABLE_RPC");
    println!("USABLE_RPC_ENDPOINT_COUNT={}", usable.len());

    let rpc_timeout = Duration::from_millis(cfg.network.timeout_ms.max(1000));
    let rotating = RotatingHttpClient::from_strings(&usable, rpc_timeout)?;
    let provider = Arc::new(Provider::new(rotating));

    let profile = match cfg.c2b_shadow.canonical_discovery_profile.as_str() {
        "liquid" => CanonicalDiscoveryProfile::Liquid,
        _ => CanonicalDiscoveryProfile::Base,
    };
    let discovery_config = CanonicalDiscoveryConfig::from_config(
        &cfg,
        profile,
        ExecutionProfile {
            chain_id: 137,
            profile_label: MAIN_PENDING_DRY_RUN_PROFILE.into(),
        },
    )
    .map_err(|e| anyhow::anyhow!("CANONICAL_DISCOVERY_CONFIG_INVALID: {e}"))?;
    println!("PROFILE={}", discovery_config.profile.label());
    println!("TOKEN_COUNT={}", discovery_config.token_count());

    let service = CanonicalDiscoveryService::new(provider.clone(), 137, discovery_config);

    let number = provider.get_block_number().await?.as_u64();
    let block = provider
        .get_block(number)
        .await?
        .ok_or_else(|| anyhow::anyhow!("CANONICAL_ONE_ROUND_ANCHOR_BLOCK_MISSING"))?;
    let hash = block
        .hash
        .ok_or_else(|| anyhow::anyhow!("CANONICAL_ONE_ROUND_ANCHOR_HASH_MISSING"))?;
    let anchor = AnchorBlock {
        number,
        hash,
        selected_from_head: number,
        confirmation_lag: 0,
    };

    let round_id = format!("diag-{}", chrono::Utc::now().timestamp());
    println!("ROUND_ID={round_id}");
    println!("ANCHOR_BLOCK={}", anchor.number);
    println!("ANCHOR_HASH={:?}", anchor.hash);

    let wall_start = Instant::now();
    let result = service.discover_at(anchor).await;
    let wall_ms = wall_start.elapsed().as_millis();

    match result {
        Ok(res) => {
            println!("EXIT_CODE=0");
            println!("WALL_CLOCK_MS={wall_ms}");
            println!("TOTAL_ROUND_MS={}", res.timing.total_ms);
            println!("ANCHOR_RESOLUTION_MS={}", res.timing.anchor_resolution_ms);
            println!("METADATA_MS={}", res.timing.metadata_ms);
            println!("QUOTE_MS={}", res.timing.quote_ms);
            println!("RANKING_MS={}", res.timing.ranking_ms);
            println!("REQUOTE_MS={}", res.timing.requote_ms);
            println!("CONTEXT_BUILD_MS={}", res.timing.context_build_ms);
            println!(
                "MATERIALIZATION_ECONOMICS_MS={}",
                res.timing.materialization_economics_ms
            );
            println!("UNATTRIBUTED_MS={}", res.timing.unattributed_ms);
            println!("QUOTES_ATTEMPTED={}", res.stats.quotes_attempted);
            println!("QUOTES_SUCCEEDED={}", res.stats.quotes_succeeded);
            println!("EDGES_CREATED={}", res.stats.edges_created);
            println!("ROUTES_DISCOVERED={}", res.stats.routes_discovered);
            println!("EXECUTABLE_ROUTES={}", res.executable_routes.len());
            println!("REJECTIONS={}", res.rejections.len());
            Ok(())
        }
        Err(err) => {
            println!("EXIT_CODE=1");
            println!("WALL_CLOCK_MS={wall_ms}");
            eprintln!("ROUND_ERROR={err:#}");
            Err(err)
        }
    }
}
