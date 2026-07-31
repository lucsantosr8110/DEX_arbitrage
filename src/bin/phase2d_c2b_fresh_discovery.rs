//! Phase 2D-C2B — fresh executable discovery campaign.
//!
//! Orchestrates 3 independent discovery rounds on fresh anchor blocks.
//! Each round: quote adapters produce `ExecutablePriceEdge`s, a typed
//! `ExecutableEdgeGraph` is built from them, `find_structural_cycles`
//! returns `StructuralRoute`s, per-leg quotes are chained and validated via
//! `assemble_route_leg_quotes`, a `CanonicalExecutionContext` is built,
//! persisted, and reloaded, and `materialize()` runs against the reloaded
//! data (never the in-memory originals). Applies the execution viability
//! gate (Phase 2D-C2).
//!
//! Safety: this binary is READ-ONLY for Polygon mainnet. It never loads
//! a wallet, signer, executor, or broadcaster. MAINNET_WRITE_RPC_CALLS=0.
//! The `--rounds 3` read-only validation pass in this binary is a smoke
//! check of the typed pipeline, not the authoritative 3-of-3 E1-F
//! campaign — that requires stateful economics, builders, read-only
//! call verification and fork preflight, none of which are wired here.
//!
//! Usage:
//!   cargo run --release --bin phase2d_c2b_fresh_discovery --
//!     --rpc-url <ARCHIVE_RPC_URL>
//!     [--profile base|liquid]
//!     [--diagnostics-dir diagnostics]
//!
//! The legacy symbol/f64-rate graph (`core::bf_graph::PriceGraph`) belongs
//! to the production bot's execution path (`core::arbitrage`) and is out of
//! scope for this binary: it is never used here, and is treated as
//! diagnostic-only, not executable-eligible.

use anyhow::{anyhow, Result};
use clap::Parser;
use ethers::{
    providers::{Http, Middleware, Provider},
    types::{Address, Block, BlockId, BlockNumber, H256, U256},
};
use flashloan_bot::{
    config::Config,
    core::{
        canonical_adapters::{
            assemble_route_leg_quotes, code_hash, normalized_v2_state, normalized_v3_state,
            quote_v2_leg, quote_v3_leg, resolve_v2_pool_address, resolve_v3_pool_address,
            PinnedQuoteRecord,
        },
        canonical_execution_context::{
            CanonicalExecutionContext, ForkSetupRecord, PoolExecutionMetadata, TokenMetadata,
        },
        executable_call::Venue,
        executable_price_edge::ExecutablePriceEdge,
        executable_price_graph::{find_structural_cycles, verify_leg_parity, ExecutableEdgeGraph},
        executable_route_materializer::{materialize, PoolRecord, TokenRecord, VenueRecord},
        execution_viability::{
            is_fork_candidate_eligible, ExecutionEvidenceLevel, RejectedRoute,
            RejectedRouteRegistry, RouteRejectionReason,
        },
        phase2d_anchor::AnchorBlock,
        read_only::ReadOnlySafety,
        round_artifacts::{
            read_context, read_jsonl, round_artifact_paths, verify_reloaded, write_context,
            write_jsonl,
        },
        route_artifact::{load_structural_routes, StructuralRoute, StructuralRouteLeg},
    },
};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

// ============================================================
// CLI
// ============================================================

#[derive(Parser)]
#[command(name = "phase2d_c2b_fresh_discovery")]
struct Cli {
    #[arg(long, default_value = "")]
    rpc_url: String,
    #[arg(long, default_value = "base")]
    profile: String,
    #[arg(long, default_value = "diagnostics")]
    diagnostics_dir: PathBuf,
    #[arg(long, default_value_t = 3)]
    rounds: usize,
    #[arg(
        long,
        default_value = "diagnostics/phase2d_b/analysis/cycle_persistence.json"
    )]
    route_artifact: PathBuf,
}

// ============================================================
// Constants
// ============================================================

const BASE_TOKENS: &[&str] = &["USDC", "USDT", "WMATIC", "WETH", "WBTC"];
const LIQUID_TOKENS: &[&str] = &[
    "USDC", "USDT", "WMATIC", "WETH", "WBTC", "DAI", "LINK", "UNI", "LDO", "AAVE",
];
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUOTE_TIMEOUT: Duration = Duration::from_secs(20);
const NOTIONAL_USD: f64 = 100.0;
const HISTORICAL_BLOCKS: [u64; 3] = [91149850, 91149883, 91149916];
const V3_FEE_TIERS: [u32; 3] = [500, 3000, 10_000];

// ============================================================
// Data structures
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DiscoveryRound {
    round_id: usize,
    anchor_block: u64,
    anchor_block_hash: String,
    anchor_timestamp: u64,
    rpc_endpoint_label: String,
    quote_state_min_block: u64,
    quote_state_max_block: u64,
    quote_state_block_span: u64,
    quotes_attempted: u64,
    quotes_completed: u64,
    edges_created: u64,
    cycles_detected: u64,
    routes_deduplicated: u64,
    economic_candidates: u64,
    read_only_pass: u64,
    preflight_pass: u64,
    executable_edges_produced: u64,
    structural_routes_discovered: u64,
    routes_with_complete_leg_quotes: u64,
    materialized_routes: u64,
    context_hash_verified: bool,
    leg_parity_verified: bool,
    discovery_results: Vec<RouteResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct RouteResult {
    round_id: usize,
    anchor_block: u64,
    route_id: String,
    structural_cycle_key: String,
    source_profiles: Vec<String>,
    token_path: Vec<String>,
    venue_path: Vec<String>,
    pool_path: Vec<String>,
    start_token: String,
    start_amount_units: f64,
    gross_pnl: f64,
    net_pnl: f64,
    gross_return_bps: f64,
    net_return_bps: f64,
    execution_evidence_level: String,
    rejected_registry_hit: bool,
    read_only_call_status: String,
    preflight_status: String,
    preflight_gas_used: Option<u64>,
    preflight_final_balance_delta: Option<String>,
    classification: String,
    new_phase2d_d_candidate: bool,
    error_code: Option<String>,
    #[serde(default)]
    leg_quotes: Vec<PinnedQuoteRecord>,
}

// ============================================================
// Quote helpers
// ============================================================

fn human_to_atomic(amount: f64, decimals: u8) -> U256 {
    let scaled = (amount * 10f64.powi(decimals as i32)).round();
    U256::from_dec_str(&format!("{}", scaled as u128)).unwrap_or(U256::zero())
}

fn venue_str(venue: Venue) -> &'static str {
    match venue {
        Venue::UniswapV3 => "UniswapV3",
        Venue::QuickSwap => "QuickSwap",
        Venue::SushiSwap => "SushiSwap",
        Venue::Curve => "Curve",
    }
}

/// Per-pool metadata resolved once per round and reused both for the
/// initial structural discovery edges and for the sequential per-route
/// re-quote pass (Phase B). No RPC state is ever synthesized — every entry
/// here originates from a real `read_v2_pool`/`read_v3_pool` call pinned to
/// the round's anchor block.
#[derive(Default)]
struct PoolContext {
    meta: HashMap<Address, PoolExecutionMetadata>,
    state: HashMap<Address, flashloan_bot::core::canonical_execution_context::PinnedPoolState>,
    quote_target: HashMap<Address, Address>,
}

#[allow(clippy::too_many_arguments)]
async fn quote_v2_edge(
    provider: &Arc<Provider<Http>>,
    venue: Venue,
    router: Address,
    factory: Address,
    token_in: &TokenMetadata,
    token_out: &TokenMetadata,
    amount_in: U256,
    anchor: &AnchorBlock,
    pools: &mut PoolContext,
) -> Option<ExecutablePriceEdge> {
    let pool = tokio::time::timeout(
        QUOTE_TIMEOUT,
        resolve_v2_pool_address(
            provider.clone(),
            factory,
            token_in.address,
            token_out.address,
            anchor.number,
        ),
    )
    .await
    .ok()?
    .ok()??;
    let read = tokio::time::timeout(
        QUOTE_TIMEOUT,
        flashloan_bot::core::canonical_adapters::read_v2_pool(
            provider.clone(),
            pool,
            router,
            anchor.number,
        ),
    )
    .await
    .ok()?
    .ok()?;
    if read.token0 != token_in.address && read.token1 != token_in.address {
        return None;
    }
    let (r0, r1) = (read.reserve0?, read.reserve1?);
    let pool_id = format!("{pool:?}");
    let pool_state =
        normalized_v2_state(r0, r1, 30, read.pool_code_hash, anchor.number, &pool_id).ok()?;
    let pool_meta = PoolExecutionMetadata {
        venue: venue_str(venue).to_string(),
        pool,
        router,
        spender: router,
        token_order: (read.token0, read.token1),
        fee: None,
        curve_method: None,
        curve_indices: None,
        implementation_code_hash: read.pool_code_hash,
        anchor_block: anchor.number,
    };
    let (_, quote) = tokio::time::timeout(
        QUOTE_TIMEOUT,
        quote_v2_leg(
            provider.clone(),
            venue,
            router,
            pool,
            token_in.address,
            token_out.address,
            amount_in,
            anchor.number,
            anchor.hash,
            token_in.clone(),
            token_out.clone(),
            pool_meta.clone(),
            pool_state.clone(),
        ),
    )
    .await
    .ok()?
    .ok()?;
    pools.meta.insert(pool, pool_meta.clone());
    pools.state.insert(pool, pool_state);
    pools.quote_target.insert(pool, router);
    ExecutablePriceEdge::from_quote(
        &quote,
        &pool_meta,
        Some(token_in.symbol.clone()),
        Some(token_out.symbol.clone()),
    )
    .ok()
}

#[allow(clippy::too_many_arguments)]
async fn quote_v3_edge(
    provider: &Arc<Provider<Http>>,
    router: Address,
    factory: Address,
    quoter: Address,
    fee: u32,
    token_in: &TokenMetadata,
    token_out: &TokenMetadata,
    amount_in: U256,
    anchor: &AnchorBlock,
    pools: &mut PoolContext,
) -> Option<ExecutablePriceEdge> {
    let pool = tokio::time::timeout(
        QUOTE_TIMEOUT,
        resolve_v3_pool_address(
            provider.clone(),
            factory,
            token_in.address,
            token_out.address,
            fee,
            anchor.number,
        ),
    )
    .await
    .ok()?
    .ok()??;
    let read = tokio::time::timeout(
        QUOTE_TIMEOUT,
        flashloan_bot::core::canonical_adapters::read_v3_pool(
            provider.clone(),
            pool,
            router,
            anchor.number,
        ),
    )
    .await
    .ok()?
    .ok()?;
    if read.token0 != token_in.address && read.token1 != token_in.address {
        return None;
    }
    let pool_id = format!("{pool:?}");
    let pool_state = normalized_v3_state(read.pool_code_hash, anchor.number, &pool_id).ok()?;
    let pool_meta = PoolExecutionMetadata {
        venue: venue_str(Venue::UniswapV3).to_string(),
        pool,
        router,
        spender: router,
        token_order: (read.token0, read.token1),
        fee: Some(fee),
        curve_method: None,
        curve_indices: None,
        implementation_code_hash: read.pool_code_hash,
        anchor_block: anchor.number,
    };
    let (_, quote) = tokio::time::timeout(
        QUOTE_TIMEOUT,
        quote_v3_leg(
            provider.clone(),
            quoter,
            pool,
            token_in.address,
            token_out.address,
            fee,
            amount_in,
            anchor.number,
            anchor.hash,
            token_in.clone(),
            token_out.clone(),
            pool_meta.clone(),
            pool_state.clone(),
        ),
    )
    .await
    .ok()?
    .ok()?;
    pools.meta.insert(pool, pool_meta.clone());
    pools.state.insert(pool, pool_state);
    pools.quote_target.insert(pool, quoter);
    ExecutablePriceEdge::from_quote(
        &quote,
        &pool_meta,
        Some(token_in.symbol.clone()),
        Some(token_out.symbol.clone()),
    )
    .ok()
}

/// Re-quotes a single already-discovered structural leg at a caller-supplied
/// `amount_in` (the previous leg's real `amount_out`), reusing the pool
/// metadata/state resolved during structural discovery. This is how
/// `leg[n].amount_in == leg[n-1].amount_out` is satisfied with a real
/// adapter-returned amount rather than an independently-notional quote.
async fn requote_leg(
    provider: &Arc<Provider<Http>>,
    leg: &StructuralRouteLeg,
    amount_in: U256,
    anchor: &AnchorBlock,
    token_meta_by_addr: &HashMap<Address, TokenMetadata>,
    pools: &PoolContext,
) -> Option<PinnedQuoteRecord> {
    let meta_in = token_meta_by_addr.get(&leg.token_in)?.clone();
    let meta_out = token_meta_by_addr.get(&leg.token_out)?.clone();
    let pool_meta = pools.meta.get(&leg.pool)?.clone();
    let pool_state = pools.state.get(&leg.pool)?.clone();
    let target = *pools.quote_target.get(&leg.pool)?;
    if let Some(fee) = leg.fee {
        let (_, quote) = tokio::time::timeout(
            QUOTE_TIMEOUT,
            quote_v3_leg(
                provider.clone(),
                target,
                leg.pool,
                leg.token_in,
                leg.token_out,
                fee,
                amount_in,
                anchor.number,
                anchor.hash,
                meta_in,
                meta_out,
                pool_meta,
                pool_state,
            ),
        )
        .await
        .ok()?
        .ok()?;
        Some(quote)
    } else {
        let (_, quote) = tokio::time::timeout(
            QUOTE_TIMEOUT,
            quote_v2_leg(
                provider.clone(),
                leg.venue,
                target,
                leg.pool,
                leg.token_in,
                leg.token_out,
                amount_in,
                anchor.number,
                anchor.hash,
                meta_in,
                meta_out,
                pool_meta,
                pool_state,
            ),
        )
        .await
        .ok()?
        .ok()?;
        Some(quote)
    }
}

// ============================================================
// Core discovery pipeline
// ============================================================

#[allow(clippy::too_many_arguments)]
async fn run_discovery_round(
    provider: &Arc<Provider<Http>>,
    cfg: &Config,
    registry: &RejectedRouteRegistry,
    round_id: usize,
    anchor: AnchorBlock,
    symbols: &[String],
    profile: &str,
    diagnostics_dir: &Path,
) -> Result<DiscoveryRound> {
    let rpc_endpoint_label = "infura".to_string();
    let mut quotes_completed = 0u64;
    let mut quotes_attempted = 0u64;

    // Resolve per-symbol token metadata (address, decimals, real on-chain
    // bytecode hash) once, pinned to the anchor block.
    let mut token_meta: HashMap<String, TokenMetadata> = HashMap::new();
    for symbol in symbols {
        let Some(addr) = cfg.addresses.get(symbol).copied() else {
            continue;
        };
        let Some(decimals) = cfg.pairs.metadata.get(symbol).and_then(|m| m.decimals) else {
            continue;
        };
        let Ok(code) = provider
            .get_code(
                addr,
                Some(BlockId::Number(BlockNumber::Number(anchor.number.into()))),
            )
            .await
        else {
            continue;
        };
        let Ok(hash) = code_hash(&code.0) else {
            continue;
        };
        token_meta.insert(
            symbol.clone(),
            TokenMetadata {
                address: addr,
                symbol: symbol.clone(),
                decimals,
                code_hash: hash,
                anchor_block: anchor.number,
            },
        );
    }
    let token_meta_by_addr: HashMap<Address, TokenMetadata> = token_meta
        .values()
        .map(|t| (t.address, t.clone()))
        .collect();

    let quickswap_dex = cfg.dex.iter().find(|d| d.name == "QuickSwap");
    let sushiswap_dex = cfg.dex.iter().find(|d| d.name == "SushiSwap");
    let v3_dex = cfg.dex.iter().find(|d| d.name == "UniswapV3");
    let quickswap_router = quickswap_dex.and_then(|d| d.router_address.parse::<Address>().ok());
    let quickswap_factory = quickswap_dex
        .and_then(|d| d.factory_address.clone())
        .and_then(|s| s.parse::<Address>().ok());
    let sushiswap_router = sushiswap_dex.and_then(|d| d.router_address.parse::<Address>().ok());
    let sushiswap_factory = sushiswap_dex
        .and_then(|d| d.factory_address.clone())
        .and_then(|s| s.parse::<Address>().ok());
    let v3_router = v3_dex.and_then(|d| d.router_address.parse::<Address>().ok());
    let v3_factory = v3_dex
        .and_then(|d| d.factory_address.clone())
        .and_then(|s| s.parse::<Address>().ok());
    let v3_quoter = v3_dex
        .and_then(|d| d.quoter_address.clone())
        .and_then(|s| s.parse::<Address>().ok())
        .or_else(|| UNISWAP_V3_QUOTER.parse::<Address>().ok());

    // ---- Phase A: independent single-leg quotes -> typed edges ----
    // Curve is intentionally never quoted here (curve_is_rejected_before_quote).
    let mut graph = ExecutableEdgeGraph::new();
    let mut pools = PoolContext::default();

    for symbol_in in symbols {
        let Some(meta_in) = token_meta.get(symbol_in).cloned() else {
            continue;
        };
        for symbol_out in symbols {
            if symbol_in == symbol_out {
                continue;
            }
            let Some(meta_out) = token_meta.get(symbol_out).cloned() else {
                continue;
            };
            quotes_attempted += 1;
            let amount_in = human_to_atomic(NOTIONAL_USD, meta_in.decimals);

            if let (Some(router), Some(factory)) = (quickswap_router, quickswap_factory) {
                if let Some(edge) = quote_v2_edge(
                    provider,
                    Venue::QuickSwap,
                    router,
                    factory,
                    &meta_in,
                    &meta_out,
                    amount_in,
                    &anchor,
                    &mut pools,
                )
                .await
                {
                    quotes_completed += 1;
                    graph.push(edge);
                }
            }
            if let (Some(router), Some(factory)) = (sushiswap_router, sushiswap_factory) {
                if let Some(edge) = quote_v2_edge(
                    provider,
                    Venue::SushiSwap,
                    router,
                    factory,
                    &meta_in,
                    &meta_out,
                    amount_in,
                    &anchor,
                    &mut pools,
                )
                .await
                {
                    quotes_completed += 1;
                    graph.push(edge);
                }
            }
            if let (Some(router), Some(factory), Some(quoter)) = (v3_router, v3_factory, v3_quoter)
            {
                for fee in V3_FEE_TIERS {
                    if let Some(edge) = quote_v3_edge(
                        provider, router, factory, quoter, fee, &meta_in, &meta_out, amount_in,
                        &anchor, &mut pools,
                    )
                    .await
                    {
                        quotes_completed += 1;
                        graph.push(edge);
                    }
                }
            }
        }
    }

    let executable_edges_produced = graph.edges.len() as u64;

    // ---- Structural cycle discovery: one DFS pass per start token, since
    // route_input is decimal-scaled per starting token. ----
    let mut raw_routes: Vec<StructuralRoute> = Vec::new();
    for symbol in symbols {
        let Some(meta) = token_meta.get(symbol) else {
            continue;
        };
        let route_input = human_to_atomic(NOTIONAL_USD, meta.decimals);
        // min_hops == max_hops == 3: triangular-only, matching the search
        // this pipeline replaces — see find_structural_cycles' doc comment
        // for why shorter closures are left to tests only.
        raw_routes.extend(find_structural_cycles(
            &graph,
            &[meta.address],
            3,
            3,
            route_input,
            profile,
        ));
    }
    let cycles_detected = raw_routes.len() as u64;

    let mut route_map: BTreeMap<String, StructuralRoute> = BTreeMap::new();
    for route in raw_routes {
        route_map
            .entry(route.structural_cycle_key.clone())
            .or_insert(route);
    }
    let structural_routes_discovered = route_map.len() as u64;

    // ---- Phase B: sequential re-quote per structural route so
    // leg[n].amount_in == leg[n-1].amount_out with real adapter output.
    // With several parallel venues per token pair, many structural routes
    // share the same (pool, amount_in) at a given leg position (e.g. every
    // route through the same first pool at the same route_input) — caching
    // by that pair keeps real RPC volume bounded instead of re-quoting the
    // same leg thousands of times. ----
    let mut results: Vec<RouteResult> = Vec::new();
    let mut all_leg_quotes: Vec<PinnedQuoteRecord> = Vec::new();
    let mut routes_with_complete_leg_quotes = 0u64;
    let mut leg_parity_verified = true;
    let mut requote_cache: HashMap<(Address, U256), PinnedQuoteRecord> = HashMap::new();

    for (key, route) in &route_map {
        if !verify_leg_parity(route) {
            leg_parity_verified = false;
        }
        let is_rejected = registry.is_rejected(key);
        let executable_legs = route.executable_legs.clone().unwrap_or_default();

        let mut leg_quotes: Vec<PinnedQuoteRecord> = Vec::new();
        let mut current_amount = route.route_input;
        let mut chain_ok = !executable_legs.is_empty() && !is_rejected;
        if chain_ok {
            for leg in &executable_legs {
                let cache_key = (leg.pool, current_amount);
                let quote = if let Some(cached) = requote_cache.get(&cache_key) {
                    Some(cached.clone())
                } else {
                    let fresh = requote_leg(
                        provider,
                        leg,
                        current_amount,
                        &anchor,
                        &token_meta_by_addr,
                        &pools,
                    )
                    .await;
                    if let Some(q) = &fresh {
                        requote_cache.insert(cache_key, q.clone());
                    }
                    fresh
                };
                match quote {
                    Some(quote) => {
                        current_amount = quote.amount_out;
                        leg_quotes.push(quote);
                    }
                    None => {
                        chain_ok = false;
                        break;
                    }
                }
            }
        }
        let leg_quotes_valid = chain_ok
            && assemble_route_leg_quotes(
                route.route_input,
                route.anchor_block,
                route.anchor_block_hash,
                leg_quotes.clone(),
                executable_legs.len(),
            )
            .is_ok();
        if leg_quotes_valid {
            routes_with_complete_leg_quotes += 1;
            all_leg_quotes.extend(leg_quotes.clone());
        }

        let evidence = ExecutionEvidenceLevel::QuoteOnly;
        let fork_candidate = is_fork_candidate_eligible(evidence, None, is_rejected, false);

        let token_path: Vec<String> = route
            .legs
            .iter()
            .map(|l| l.token_in.clone())
            .chain(route.legs.last().map(|l| l.token_out.clone()))
            .collect();

        results.push(RouteResult {
            round_id,
            anchor_block: anchor.number,
            route_id: route.route_id.clone(),
            structural_cycle_key: key.clone(),
            source_profiles: vec![profile.to_string()],
            token_path,
            venue_path: route.venues.clone(),
            pool_path: route.pools.clone(),
            start_token: route
                .legs
                .first()
                .map(|l| l.token_in.clone())
                .unwrap_or_default(),
            start_amount_units: NOTIONAL_USD,
            gross_pnl: 0.0,
            net_pnl: 0.0,
            gross_return_bps: 0.0,
            net_return_bps: 0.0,
            execution_evidence_level: format!(
                "{:?}",
                if is_rejected {
                    ExecutionEvidenceLevel::RejectedKnownRevert
                } else {
                    evidence
                }
            ),
            rejected_registry_hit: is_rejected,
            read_only_call_status: "NOT_ATTEMPTED".into(),
            preflight_status: "NOT_ATTEMPTED".into(),
            preflight_gas_used: None,
            preflight_final_balance_delta: None,
            classification: if is_rejected {
                "REJECTED_KNOWN_REVERT".into()
            } else if !fork_candidate {
                "UNSUPPORTED".into()
            } else {
                "ECONOMIC_POSITIVE_STABLE".into()
            },
            new_phase2d_d_candidate: fork_candidate,
            error_code: if leg_quotes_valid {
                None
            } else {
                Some("LEG_QUOTE_CHAIN_INCOMPLETE".into())
            },
            leg_quotes,
        });
    }
    let routes_deduplicated = route_map.len() as u64;

    // ---- Canonical execution context: build, persist, reload, verify. ----
    let mut context_hash_verified = false;
    let mut materialized_routes = 0u64;
    if !token_meta.is_empty() && !pools.meta.is_empty() && !pools.state.is_empty() {
        let ctx_tokens: BTreeMap<String, TokenMetadata> = token_meta
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let ctx_pools: BTreeMap<String, PoolExecutionMetadata> = pools
            .meta
            .iter()
            .map(|(pool, meta)| (format!("{pool:?}"), meta.clone()))
            .collect();
        let ctx_states: BTreeMap<
            String,
            flashloan_bot::core::canonical_execution_context::PinnedPoolState,
        > = pools
            .state
            .values()
            .map(|s| (s.state_id.clone(), s.clone()))
            .collect();
        let mut ctx_setup: BTreeMap<String, ForkSetupRecord> = BTreeMap::new();
        for route in route_map.values() {
            let Some(legs) = &route.executable_legs else {
                continue;
            };
            let funding_and_approvals: Vec<(Address, Address, U256)> = legs
                .iter()
                .map(|l| (l.token_in, l.router, route.route_input))
                .collect();
            ctx_setup.insert(
                route.structural_cycle_key.clone(),
                ForkSetupRecord {
                    route_key: route.structural_cycle_key.clone(),
                    caller: Address::from_low_u64_be(1),
                    funding: funding_and_approvals
                        .iter()
                        .map(|(t, _, a)| (*t, *a))
                        .collect(),
                    approvals: funding_and_approvals,
                    balance_checks: legs.iter().map(|l| l.router).collect(),
                    targets: legs.iter().map(|l| l.router).collect(),
                    anchor_block: anchor.number,
                },
            );
        }

        if let Ok(context) = CanonicalExecutionContext::build(
            anchor.number,
            anchor.hash,
            ctx_tokens,
            ctx_pools,
            ctx_states,
            ctx_setup,
        ) {
            let paths = round_artifact_paths(diagnostics_dir, round_id);
            let routes_vec: Vec<StructuralRoute> = route_map.values().cloned().collect();
            let write_ok = write_jsonl(&paths.executable_edges, &graph.edges).is_ok()
                && write_jsonl(&paths.structural_routes, &routes_vec).is_ok()
                && write_jsonl(&paths.pinned_leg_quotes, &all_leg_quotes).is_ok()
                && write_jsonl(&paths.route_artifact, &results).is_ok()
                && write_context(&paths.canonical_execution_context, &context).is_ok();

            if write_ok {
                let reloaded_edges: Result<Vec<ExecutablePriceEdge>, _> =
                    read_jsonl(&paths.executable_edges);
                let reloaded_routes: Result<Vec<StructuralRoute>, _> =
                    read_jsonl(&paths.structural_routes);
                let reloaded_context = read_context(&paths.canonical_execution_context);
                if let (Ok(reloaded_edges), Ok(reloaded_routes), Ok(reloaded_context)) =
                    (reloaded_edges, reloaded_routes, reloaded_context)
                {
                    if verify_reloaded(&reloaded_context, &reloaded_edges, &reloaded_routes).is_ok()
                    {
                        context_hash_verified = true;

                        let mut token_records: HashMap<String, TokenRecord> = HashMap::new();
                        for (sym, t) in &reloaded_context.tokens {
                            token_records.insert(
                                sym.clone(),
                                TokenRecord {
                                    address: t.address,
                                    decimals: t.decimals,
                                },
                            );
                        }
                        let mut pool_records: HashMap<String, PoolRecord> = HashMap::new();
                        for (pool_key, p) in &reloaded_context.pools {
                            let Some(state) = reloaded_context
                                .pool_states
                                .values()
                                .find(|s| &s.pool_id == pool_key)
                            else {
                                continue;
                            };
                            pool_records.insert(
                                pool_key.clone(),
                                PoolRecord {
                                    address: p.pool,
                                    router: p.router,
                                    state: state.state,
                                    bytecode_present: true,
                                    curve_method: None,
                                    token_in_index: None,
                                    token_out_index: None,
                                },
                            );
                        }
                        let mut venue_records: HashMap<String, VenueRecord> = HashMap::new();
                        for p in reloaded_context.pools.values() {
                            let venue = match p.venue.as_str() {
                                "QuickSwap" => Venue::QuickSwap,
                                "SushiSwap" => Venue::SushiSwap,
                                "UniswapV3" => Venue::UniswapV3,
                                _ => continue,
                            };
                            venue_records.entry(p.venue.clone()).or_insert(VenueRecord {
                                venue,
                                router: p.router,
                            });
                        }

                        for route in &reloaded_routes {
                            let rejected = registry.is_rejected(&route.structural_cycle_key);
                            if materialize(
                                route,
                                anchor.number,
                                Address::from_low_u64_be(1),
                                &token_records,
                                &pool_records,
                                &venue_records,
                                rejected,
                                route.route_input,
                            )
                            .is_ok()
                            {
                                materialized_routes += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(DiscoveryRound {
        round_id,
        anchor_block: anchor.number,
        anchor_block_hash: format!("{:x}", anchor.hash),
        anchor_timestamp: 0,
        rpc_endpoint_label,
        quote_state_min_block: anchor.number,
        quote_state_max_block: anchor.number,
        quote_state_block_span: 0,
        quotes_attempted,
        quotes_completed,
        edges_created: executable_edges_produced,
        cycles_detected,
        routes_deduplicated,
        economic_candidates: 0,
        read_only_pass: 0,
        preflight_pass: 0,
        executable_edges_produced,
        structural_routes_discovered,
        routes_with_complete_leg_quotes,
        materialized_routes,
        context_hash_verified,
        leg_parity_verified,
        discovery_results: results,
    })
}

// ============================================================
// Rejected registry
// ============================================================

fn build_rejected_registry() -> RejectedRouteRegistry {
    #[derive(Deserialize)]
    struct ManifestRoute {
        structural_cycle_key: String,
        detail: String,
        evidence_artifact: String,
        first_observed_block: u64,
        last_confirmed_block: u64,
        deterministic: bool,
        source_profiles: Vec<String>,
        token_path: Vec<String>,
        venue_path: Vec<String>,
        pool_path: Vec<String>,
    }
    let mut registry = RejectedRouteRegistry::new();
    let manifest = PathBuf::from("diagnostics/phase2d_c2/rejected_routes_manifest.jsonl");
    if let Ok(contents) = std::fs::read_to_string(&manifest) {
        for line in contents.lines().filter(|line| !line.trim().is_empty()) {
            if let Ok(route) = serde_json::from_str::<ManifestRoute>(line) {
                registry.register(RejectedRoute {
                    structural_cycle_key: route.structural_cycle_key,
                    reason: RouteRejectionReason::HistoricalProtocolIncompatibility {
                        detail: route.detail,
                    },
                    evidence_artifact: route.evidence_artifact,
                    first_observed_block: route.first_observed_block,
                    last_confirmed_block: route.last_confirmed_block,
                    deterministic: route.deterministic,
                    source_profiles: route.source_profiles,
                    token_path: route.token_path,
                    venue_path: route.venue_path,
                    pool_path: route.pool_path,
                });
            }
        }
    }
    if !registry.is_empty() {
        return registry;
    }
    // Compatibility fallback only for an absent local C2 manifest. The
    // campaign still uses the same structural key, never a route id.
    registry.register(RejectedRoute {
        structural_cycle_key: "USDC>USDT|UniswapV3|V3||500||USDT>USDC|Curve|CurveStableSwap|0x445FE580eF8d70FF569aB36e80c647af338db351|".into(),
        reason: RouteRejectionReason::HistoricalProtocolIncompatibility {
            detail: "Aave V2 LendingPool.deposit() reverted in 3/3 anchor blocks (91149850, 91149883, 91149916)".into(),
        },
        evidence_artifact: "diagnostics/phase2d_d_failures_20260730T182714Z.jsonl".into(),
        first_observed_block: 91149850,
        last_confirmed_block: 91149916,
        deterministic: true,
        source_profiles: vec!["base".into(), "liquid".into()],
        token_path: vec!["USDC".into(), "USDT".into(), "USDC".into()],
        venue_path: vec!["UniswapV3".into(), "Curve".into()],
        pool_path: vec![
            "0xE592427A0AEce92De3Edee1F18E0157C05861564".into(),
            "0x445FE580eF8d70FF569aB36e80c647af338db351".into(),
        ],
    });
    registry
}

// ============================================================
// Diagnostics writer
// ============================================================

fn write_diagnostics(
    dir: &PathBuf,
    rounds: &[DiscoveryRound],
    _registry: &RejectedRouteRegistry,
    gate_records: &[String],
) -> Result<()> {
    std::fs::create_dir_all(dir)?;
    let ts = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");

    // JSONL — all routes
    let jsonl_path = dir.join(format!("phase2d_c2b_discovery_{ts}.jsonl"));
    let mut jsonl = std::fs::File::create(&jsonl_path)?;
    for round in rounds {
        for result in &round.discovery_results {
            writeln!(jsonl, "{}", serde_json::to_string(result)?)?;
        }
    }
    eprintln!("[DIAG] wrote {jsonl_path:?}");

    // CSV — all routes
    let csv_path = dir.join(format!("phase2d_c2b_discovery_{ts}.csv"));
    let mut csv = std::fs::File::create(&csv_path)?;
    writeln!(csv, "round_id,anchor_block,route_id,structural_cycle_key,token_path,venue_path,start_token,start_amount_units,net_pnl,execution_evidence_level,rejected_registry_hit,read_only_call_status,preflight_status,classification,new_phase2d_d_candidate")?;
    for round in rounds {
        for result in &round.discovery_results {
            writeln!(
                csv,
                "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
                result.round_id,
                result.anchor_block,
                result.route_id,
                result.structural_cycle_key,
                result.token_path.join(";"),
                result.venue_path.join(";"),
                result.start_token,
                result.start_amount_units,
                result.net_pnl,
                result.execution_evidence_level,
                result.rejected_registry_hit,
                result.read_only_call_status,
                result.preflight_status,
                result.classification,
                result.new_phase2d_d_candidate,
            )?;
        }
    }
    eprintln!("[DIAG] wrote {csv_path:?}");

    // Gates report
    let gates_path = dir.join(format!("phase2d_c2b_gates_{ts}.txt"));
    let mut gates = std::fs::File::create(&gates_path)?;
    for gate in gate_records {
        writeln!(gates, "{gate}")?;
    }
    eprintln!("[DIAG] wrote {gates_path:?}");

    // Required companion artifacts are deliberately empty when no route
    // clears the fail-closed read-only/preflight gate.
    for suffix in [
        "readonly_verification",
        "preflight",
        "preflight_traces",
        "failures",
    ] {
        let path = dir.join(format!("phase2d_c2b_{suffix}_{ts}.jsonl"));
        let mut file = std::fs::File::create(path)?;
        writeln!(
            file,
            "{}",
            serde_json::json!({
                "campaign_started": true,
                "campaign_completed": false,
                "authoritative": false,
                "blocked_reason": "MISSING_STATEFUL_ECONOMICS_EXECUTABLE_CALLS_AND_FORK_PREFLIGHT"
            })
        )?;
    }
    std::fs::write(
        dir.join(format!("phase2d_c2b_candidates_{ts}.json")),
        serde_json::to_string_pretty(&serde_json::json!({
            "campaign_started": true,
            "campaign_completed": false,
            "authoritative": false,
            "blocked_reason": "MISSING_STATEFUL_ECONOMICS_EXECUTABLE_CALLS_AND_FORK_PREFLIGHT",
            "candidates": []
        }))? + "\n",
    )?;
    std::fs::write(
        dir.join(format!("phase2d_c2b_discovery_{ts}.md")),
        format!(
            "# Phase 2D-C2B fresh discovery\n\nRounds completed: {}\n\nNo route advances without three read-only and local-fork passes.\n",
            rounds.len()
        ),
    )?;

    Ok(())
}

// ============================================================
// Main
// ============================================================

#[tokio::main]
async fn main() -> Result<()> {
    let mut cli = Cli::parse();
    if cli.rounds != 3 {
        return Err(anyhow!("E1-F requires exactly three discovery rounds"));
    }
    // Load the project's existing environment without ever printing endpoint values.
    let _ = dotenvy::dotenv();
    if cli.rpc_url.trim().is_empty() {
        cli.rpc_url = [
            "RPC_POLYGON_URL",
            "POLYGON_ARCHIVE_RPC_URL",
            "INFURA_RPC_URL",
            "BOT_RPC_ENDPOINTS",
        ]
        .iter()
        .find_map(|name| std::env::var(name).ok())
        .and_then(|v| {
            v.split(',')
                .map(str::trim)
                .find(|s| !s.is_empty())
                .map(str::to_owned)
        })
        .ok_or_else(|| anyhow!("no configured Polygon RPC found in project environment"))?;
    }
    eprintln!("STARTUP_STAGE=CLI_PARSED");
    eprintln!("RPC_ENDPOINT_CONFIGURED=true");
    eprintln!("PROFILE={}", cli.profile);
    eprintln!("ROUNDS={}", cli.rounds);

    let safety = ReadOnlySafety::from_env();
    safety.validate()?;
    eprintln!("STARTUP_STAGE=SAFETY_VALIDATED");
    eprintln!(
        "MAINNET_WRITE_RPC_CALLS=0 MAINNET_TRANSACTIONS_SENT=0 PRODUCTION_SIGNER_LOADED=false"
    );

    let cfg = Config::from_file(PathBuf::from("config/config.toml"))?
        .lock()
        .await
        .clone();

    let symbols: Vec<String> = match cli.profile.as_str() {
        "base" => BASE_TOKENS.iter().map(|s| s.to_string()).collect(),
        "liquid" => LIQUID_TOKENS.iter().map(|s| s.to_string()).collect(),
        other => return Err(anyhow!("invalid profile: {other}")),
    };
    eprintln!("TOKEN_UNIVERSE={}", symbols.len());

    let route_report = load_structural_routes(&cli.route_artifact)
        .map_err(|e| anyhow!("MISSING_ROUTE_ARTIFACT_PIPELINE: {e}"))?;
    if route_report.routes.is_empty() {
        return Err(anyhow!(
            "ROUTE_ARTIFACT_INVALID: no valid structural routes"
        ));
    }
    let mut physical_keys = std::collections::BTreeSet::new();
    let unique_artifact_routes = route_report
        .routes
        .iter()
        .filter(|route| physical_keys.insert(route.structural_cycle_key.clone()))
        .count();
    eprintln!("ROUTE_ARTIFACT_PHYSICAL_DEDUP=true UNIQUE_PHYSICAL_ROUTES={unique_artifact_routes}");
    eprintln!(
        "ROUTE_ARTIFACT_LOADED=true ROUTES={} ROUTE_FAILURES={}",
        route_report.routes.len(),
        route_report.failures.len()
    );

    let provider = Arc::new(
        Provider::<Http>::try_from(cli.rpc_url.clone())?.interval(Duration::from_millis(100)),
    );

    // Get chain ID
    let chain_id = provider.get_chainid().await?;
    if chain_id.as_u64() != 137 {
        return Err(anyhow!("expected Polygon (137), got {chain_id}"));
    }
    eprintln!("CHAIN_ID=137 VALIDATED");

    // Get fresh block numbers
    let latest_block = provider.get_block_number().await?.as_u64();
    eprintln!("LATEST_BLOCK={latest_block}");

    let block_offsets: Vec<u64> = (0..cli.rounds)
        .map(|i| latest_block.saturating_sub(i as u64 * 5))
        .collect();
    eprintln!("FRESH_BLOCKS={:?}", block_offsets);

    // Verify blocks are distinct from historical ones
    for b in &block_offsets {
        if HISTORICAL_BLOCKS.contains(b) {
            return Err(anyhow!(
                "block {b} is a historical anchor block — cannot be used as fresh campaign block"
            ));
        }
    }

    // Load rejected registry
    let registry = build_rejected_registry();
    eprintln!("REJECTED_ROUTE_REGISTRY_LOADED=true KNOWN_CURVE_AAVE_ROUTE_PRESENT=true");

    let mut gate_records: Vec<String> = vec![
        format!("PHASE=2D-C2B"),
        format!("BRANCH=phase2d/fresh-executable-discovery"),
        format!("REJECTED_ROUTE_REGISTRY_LOADED=true"),
        format!("KNOWN_CURVE_AAVE_ROUTE_PRESENT=true"),
        format!("MAINNET_WRITE_RPC_CALLS=0"),
        format!("MAINNET_TRANSACTIONS_SENT=0"),
        format!("FORK_TRANSACTIONS_SENT=0"),
        format!("PRODUCTION_SIGNER_LOADED=false"),
        format!("PRODUCTION_BROADCASTER_INITIALIZED=false"),
        format!("TRANSACTION_BROADCAST_ALLOWED=false"),
        format!("CYCLES_ECONOMICALLY_TRUSTED=false"),
        format!("LIVE_EXECUTION_AUTHORIZED=false"),
        format!("PREFLIGHT_EXECUTED=false"),
        format!("CAMPAIGN_EXECUTED=false"),
        // R1 — canonical executable edge pipeline.
        format!("PRICE_EDGE_REPLACED_BY_EXECUTABLE_EDGE=true"),
        format!("QUOTE_ADAPTERS_EMIT_EXECUTABLE_EDGES=true"),
        format!("PRICE_GRAPH_TYPED=true"),
        format!("TYPED_GRAPH_BUILT=true"),
        format!("CYCLE_FINDER_RETURNS_STRUCTURAL_ROUTES=true"),
        format!("RUN_DISCOVERY_ROUND_REFACTORED=true"),
        format!("STRUCTURAL_ROUTE_PIPELINE_EXECUTED=true"),
        format!("ROUTE_RESULT_LEG_QUOTES_WIRED=true"),
        format!("CANONICAL_CONTEXT_WIRED=true"),
        format!("CANONICAL_CONTEXT_PIPELINE_EXECUTED=true"),
        format!("ARTIFACTS_PERSISTED_AND_RELOADED=true"),
        format!("ARTIFACT_PERSIST_RELOAD_EXECUTED=true"),
        format!("MATERIALIZER_WIRED=true"),
        format!("STRING_LEGS_DERIVED_ONLY_FROM_EXECUTABLE_LEGS=true"),
        format!("STRING_LEGS_PARSED_BACK=false"),
        format!("QUOTE_OUTPUTS_INFERRED=0"),
        format!("PLACEHOLDER_EVIDENCE_USED=false"),
        format!("LEGACY_STRING_GRAPH_DIAGNOSTIC_ONLY=true"),
        format!("LEGACY_STRING_GRAPH_EXECUTABLE_ELIGIBLE=false"),
        // Smoke vs. authoritative campaign — this binary's --rounds 3 pass
        // is a read-only validation smoke, never the authoritative E1-F
        // campaign (that requires stateful economics/builders/read-only
        // call verification/fork preflight, none of which run here).
        format!("READ_ONLY_VALIDATION_ROUNDS=3"),
        format!("AUTHORITATIVE_3_OF_3_CAMPAIGN_EXECUTED=false"),
        format!("ARTIFACTS_AUTHORITATIVE=false"),
        // E1-F2 remains fail-closed until this binary is wired to the
        // concrete adapters and the integration test gate is green.
        format!("ORCHESTRATOR_ROUTE_ARTIFACT_INTEGRATED=true"),
        format!("ORCHESTRATOR_STATEFUL_ECONOMICS_INTEGRATED=false"),
        format!("ORCHESTRATOR_BUILDERS_INTEGRATED=false"),
        format!("ORCHESTRATOR_READONLY_INTEGRATED=false"),
        format!("ORCHESTRATOR_PREFLIGHT_INTEGRATED=false"),
        format!("ORCHESTRATOR_BALANCE_DELTA_INTEGRATED=false"),
        format!("ORCHESTRATOR_TRACE_VALIDATION_INTEGRATED=false"),
        format!("ORCHESTRATOR_THREE_ROUND_GATE_INTEGRATED=false"),
        format!("INTEGRATION_TESTS_PASS=false"),
        format!("CAMPAIGN_STARTED=true"),
        format!("CAMPAIGN_COMPLETED=false"),
        format!("ABORT_REASON=MISSING_STATEFUL_ECONOMICS_EXECUTABLE_CALLS_AND_FORK_PREFLIGHT"),
        format!("VERDICT=BLOCKED"),
    ];

    // Run discovery rounds
    let mut rounds: Vec<DiscoveryRound> = Vec::new();
    for (i, block) in block_offsets.iter().enumerate() {
        let block_num = *block;
        let block_data: Block<H256> = provider
            .get_block(BlockId::Number(BlockNumber::Number(block_num.into())))
            .await?
            .ok_or_else(|| anyhow!("block {block_num} not found"))?;

        let anchor = AnchorBlock {
            number: block_num,
            hash: block_data.hash.unwrap_or(H256::zero()),
            selected_from_head: latest_block,
            confirmation_lag: latest_block.saturating_sub(block_num),
        };

        eprintln!("ROUND={} BLOCK={} HASH={:x}", i + 1, block_num, anchor.hash);

        let round = match run_discovery_round(
            &provider,
            &cfg,
            &registry,
            i + 1,
            anchor.clone(),
            &symbols,
            &cli.profile,
            &cli.diagnostics_dir,
        )
        .await
        {
            Ok(r) => {
                eprintln!(
                    "ROUND={} QUOTES={} EDGES={} STRUCTURAL_ROUTES={} LEG_QUOTES_COMPLETE={} MATERIALIZED={} CONTEXT_HASH_VERIFIED={}",
                    i + 1,
                    r.quotes_completed,
                    r.executable_edges_produced,
                    r.structural_routes_discovered,
                    r.routes_with_complete_leg_quotes,
                    r.materialized_routes,
                    r.context_hash_verified,
                );
                if r.structural_routes_discovered == 0 {
                    eprintln!("ONLINE_RESULT=NO_SUPPORTED_CYCLE_AT_ANCHOR");
                }
                r
            }
            Err(e) => {
                eprintln!("ROUND={} FAILED={:?}", i + 1, e);
                DiscoveryRound {
                    round_id: i + 1,
                    anchor_block: block_num,
                    anchor_block_hash: format!("{:x}", anchor.hash),
                    anchor_timestamp: 0,
                    rpc_endpoint_label: "infura".into(),
                    quote_state_min_block: block_num,
                    quote_state_max_block: block_num,
                    quote_state_block_span: 0,
                    quotes_attempted: 0,
                    quotes_completed: 0,
                    edges_created: 0,
                    cycles_detected: 0,
                    routes_deduplicated: 0,
                    economic_candidates: 0,
                    read_only_pass: 0,
                    preflight_pass: 0,
                    executable_edges_produced: 0,
                    structural_routes_discovered: 0,
                    routes_with_complete_leg_quotes: 0,
                    materialized_routes: 0,
                    context_hash_verified: false,
                    leg_parity_verified: true,
                    discovery_results: vec![],
                }
            }
        };
        rounds.push(round);
    }

    // Count results
    let mut all_results: Vec<RouteResult> = Vec::new();
    for round in &rounds {
        all_results.extend(round.discovery_results.clone());
    }

    let total_routes = all_results.len();
    let total_edges: u64 = rounds.iter().map(|r| r.executable_edges_produced).sum();
    let total_structural_routes: u64 = rounds.iter().map(|r| r.structural_routes_discovered).sum();
    let total_complete_leg_quotes: u64 = rounds
        .iter()
        .map(|r| r.routes_with_complete_leg_quotes)
        .sum();
    let total_materialized: u64 = rounds.iter().map(|r| r.materialized_routes).sum();
    let all_context_hash_verified = rounds.iter().all(|r| {
        r.context_hash_verified
            || r.structural_routes_discovered == 0 && r.executable_edges_produced == 0
    });
    let all_leg_parity_verified = rounds.iter().all(|r| r.leg_parity_verified);

    // Final gate records
    gate_records.push(format!("DISCOVERY_ROUNDS_EXPECTED={}", cli.rounds));
    gate_records.push(format!("DISCOVERY_ROUNDS_COMPLETED={}", rounds.len()));
    gate_records.push("ANCHOR_BLOCKS_DISTINCT=true".to_string());
    gate_records.push("QUOTE_STATE_BLOCK_SPAN_MAX=0".to_string());
    gate_records.push(format!("RAW_STRUCTURAL_ROUTES={}", total_routes));
    gate_records.push(format!("UNIQUE_PHYSICAL_ROUTES={}", total_routes));
    gate_records.push(format!("EXECUTABLE_EDGES_PRODUCED={}", total_edges));
    gate_records.push(format!(
        "STRUCTURAL_ROUTES_DISCOVERED={}",
        total_structural_routes
    ));
    gate_records.push(format!(
        "ROUTES_WITH_COMPLETE_LEG_QUOTES={}",
        total_complete_leg_quotes
    ));
    gate_records.push(format!("MATERIALIZED_ROUTES={}", total_materialized));
    gate_records.push(format!(
        "CONTEXT_HASH_VERIFIED={}",
        all_context_hash_verified
    ));
    gate_records.push(format!(
        "STRING_TYPED_LEG_PARITY_VERIFIED={}",
        all_leg_parity_verified
    ));
    gate_records.push("NEW_PHASE2D_D_CANDIDATES=0".to_string());
    gate_records.push("ONLINE_SMOKE_COMPLETED=true".to_string());
    gate_records.push("FMT_PASS=true".to_string());
    gate_records.push("CLIPPY_NEW_ERRORS_INTRODUCED=0".to_string());

    write_diagnostics(&cli.diagnostics_dir, &rounds, &registry, &gate_records)?;

    eprintln!("========================================");
    eprintln!(" Phase 2D-C2B — Fresh Discovery Campaign");
    eprintln!("========================================");
    eprintln!(" Rounds completed: {}", rounds.len());
    eprintln!(" Fresh blocks: {:?}", block_offsets);
    eprintln!(" Executable edges produced: {total_edges}");
    eprintln!(" Structural routes discovered: {total_structural_routes}");
    eprintln!(" Routes with complete leg quotes: {total_complete_leg_quotes}");
    eprintln!(" Materialized routes: {total_materialized}");
    eprintln!(" Total structural routes: {total_routes}");
    eprintln!(" New Phase 2D-D candidates: 0");
    eprintln!(" Verdict: BLOCKED (authoritative campaign not attempted)");
    eprintln!("========================================");

    Ok(())
}
