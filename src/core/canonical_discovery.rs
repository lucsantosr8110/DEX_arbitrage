//! Canonical single-anchor discovery boundary.
//!
//! The service owns anchor validation and exposes one authoritative entry
//! point for scheduler callers.  `discover_at` runs the real operational
//! pipeline pinned to that anchor: pool metadata resolution, on-chain state
//! reads, V2/V3 quotes, `ExecutablePriceEdge`/`ExecutableEdgeGraph`
//! construction, structural cycle discovery, sequential per-route amount
//! propagation, `CanonicalExecutionContext` construction, route
//! materialization, and pure (no-RPC) route economics. Diagnostic
//! formatting, filesystem artifact persistence, Anvil fork execution, and
//! the rejected-route manifest all stay in binaries; no signer or
//! broadcaster is accepted here.

use crate::config::Config;
use crate::core::{
    c2b_round::RoundEvidence,
    canonical_adapters::{
        assemble_route_leg_quotes, code_hash, normalized_v2_state, normalized_v3_state,
        quote_v2_leg, quote_v3_leg, read_v2_pool, read_v3_pool, resolve_v2_pool_address,
        resolve_v3_pool_address, PinnedQuoteRecord,
    },
    canonical_execution_context::{
        CanonicalExecutionContext, ForkSetupRecord, PinnedPoolState, PoolExecutionMetadata,
        TokenMetadata,
    },
    executable_call::Venue,
    executable_price_edge::ExecutablePriceEdge,
    executable_price_graph::{find_structural_cycles, ExecutableEdgeGraph},
    executable_route_materializer::{materialize, PoolRecord, TokenRecord, VenueRecord},
    execution_profile::ExecutionProfile,
    fresh_economics::{FreshEconomicEvaluator, SimulationContext, StatefulRouteEvaluator},
    phase2d_anchor::AnchorBlock,
    pool_state_sim::SimulatedPoolState,
    route_artifact::StructuralRoute,
};
use anyhow::{anyhow, Result};
use ethers::{
    providers::Middleware,
    types::{Address, BlockId, BlockNumber, H256, U256},
};
use std::{
    collections::{BTreeMap, HashMap},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

pub type PinnedAnchor = AnchorBlock;

// ============================================================
// Operational pipeline constants (token universe, venue defaults)
// ============================================================

const BASE_TOKENS: &[&str] = &["USDC", "USDT", "WMATIC", "WETH", "WBTC"];
const DISCOVERY_PROFILE: &str = "base";
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUOTE_TIMEOUT: Duration = Duration::from_secs(20);
const NOTIONAL_USD: f64 = 100.0;
const V3_FEE_TIERS: [u32; 3] = [500, 3000, 10_000];
const CANONICAL_CALLER: fn() -> Address = || Address::from_low_u64_be(1);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRejection {
    pub stage: &'static str,
    pub reason: String,
    pub anchor: PinnedAnchor,
    pub structural_cycle_key: Option<String>,
}

/// Counters for one `discover_at` round. Every field reflects a stage that
/// actually ran — nothing here is inferred or pre-populated.
#[derive(Debug, Clone, Default)]
pub struct DiscoveryStats {
    pub quotes_attempted: u64,
    pub quotes_succeeded: u64,
    pub edges_created: u64,
    /// Raw structural cycles found before deduplication by
    /// `structural_cycle_key` (one DFS pass runs per start token, so the
    /// same physical cycle can surface more than once here).
    pub cycles_detected: u64,
    pub routes_discovered: u64,
    pub routes_materialized: u64,
    pub economics_evaluated: u64,
}

#[derive(Debug, Clone)]
pub struct CanonicalDiscoveryResult {
    pub anchor: PinnedAnchor,
    /// Only operational evidence belongs here. Fork receipts/traces are
    /// deliberately absent and remain diagnostic-audit data.
    pub round_evidence: Vec<RoundEvidence>,
    pub executable_routes: Vec<crate::core::executable_route_materializer::ExecutableRoutePlan>,
    pub economically_positive: Vec<crate::core::executable_route_materializer::ExecutableRoutePlan>,
    pub rejections: Vec<CanonicalRejection>,
    pub stats: DiscoveryStats,
    /// Leg-quote-complete structural routes keyed by `structural_cycle_key`,
    /// with the real Phase-B re-quote chain and the pinned pool states used
    /// to produce them. Exposed so a caller's own fork-audit stage (Anvil
    /// execution, receipts, traces — never performed by this service) can
    /// build its `RoutePlan` without re-running quotes/graph/cycle
    /// discovery a second time.
    pub structural_routes: BTreeMap<String, StructuralRoute>,
    pub leg_quotes: HashMap<String, Vec<PinnedQuoteRecord>>,
    pub pool_states: HashMap<Address, SimulatedPoolState>,
}

/// Per-pool metadata resolved once per round and reused both for the
/// initial structural discovery edges and for the sequential per-route
/// re-quote pass. No RPC state is ever synthesized — every entry here
/// originates from a real `read_v2_pool`/`read_v3_pool` call pinned to the
/// round's anchor block.
#[derive(Default)]
struct PoolContext {
    meta: HashMap<Address, PoolExecutionMetadata>,
    state: HashMap<Address, PinnedPoolState>,
    quote_target: HashMap<Address, Address>,
}

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

#[allow(clippy::too_many_arguments)]
async fn quote_v2_edge<M: Middleware>(
    provider: &Arc<M>,
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
        read_v2_pool(provider.clone(), pool, router, anchor.number),
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
async fn quote_v3_edge<M: Middleware>(
    provider: &Arc<M>,
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
        read_v3_pool(provider.clone(), pool, router, anchor.number),
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
async fn requote_leg<M: Middleware>(
    provider: &Arc<M>,
    leg: &crate::core::route_artifact::StructuralRouteLeg,
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

/// Read-only provider-backed canonical discovery service.  Discovery
/// adapters are added to this service; callers cannot bypass pinned-anchor
/// validation.
pub struct CanonicalDiscoveryService<M> {
    provider: Arc<M>,
    expected_chain_id: u64,
}

impl<M> CanonicalDiscoveryService<M>
where
    M: Middleware,
    M::Error: 'static,
{
    pub fn new(provider: Arc<M>, expected_chain_id: u64) -> Self {
        Self {
            provider,
            expected_chain_id,
        }
    }

    pub async fn discover_at(&self, anchor: PinnedAnchor) -> Result<CanonicalDiscoveryResult> {
        if anchor.hash == H256::zero() {
            return Err(anyhow!("CANONICAL_ANCHOR_ZERO_HASH"));
        }
        let chain_id = self.provider.get_chainid().await?.as_u64();
        if chain_id != self.expected_chain_id {
            return Err(anyhow!("CANONICAL_CHAIN_ID_MISMATCH"));
        }
        let observed = self
            .provider
            .get_block(BlockId::Number(BlockNumber::Number(anchor.number.into())))
            .await?
            .ok_or_else(|| anyhow!("CANONICAL_ANCHOR_BLOCK_MISSING"))?;
        if observed.hash != Some(anchor.hash) {
            return Err(anyhow!("CANONICAL_ANCHOR_HASH_MISMATCH"));
        }

        let cfg = Config::from_file(PathBuf::from("config/config.toml"))?
            .lock()
            .await
            .clone();
        let symbols: Vec<String> = BASE_TOKENS.iter().map(|s| s.to_string()).collect();
        let profile = DISCOVERY_PROFILE;

        let mut stats = DiscoveryStats::default();
        let mut rejections: Vec<CanonicalRejection> = Vec::new();
        let mut round_evidence: Vec<RoundEvidence> = Vec::new();
        let mut executable_routes = Vec::new();
        let mut economically_positive = Vec::new();

        // ---- Pool/token metadata: real on-chain code hash + decimals,
        // pinned to the anchor block. ----
        let mut token_meta: HashMap<String, TokenMetadata> = HashMap::new();
        for symbol in &symbols {
            let Some(addr) = cfg.addresses.get(symbol).copied() else {
                continue;
            };
            let Some(decimals) = cfg.pairs.metadata.get(symbol).and_then(|m| m.decimals) else {
                continue;
            };
            let Ok(code) = self
                .provider
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

        // ---- Phase A: independent single-leg quotes -> typed edges.
        // Curve is intentionally never quoted here. ----
        let mut graph = ExecutableEdgeGraph::new();
        let mut pools = PoolContext::default();

        for symbol_in in &symbols {
            let Some(meta_in) = token_meta.get(symbol_in).cloned() else {
                continue;
            };
            for symbol_out in &symbols {
                if symbol_in == symbol_out {
                    continue;
                }
                let Some(meta_out) = token_meta.get(symbol_out).cloned() else {
                    continue;
                };
                stats.quotes_attempted += 1;
                let amount_in = human_to_atomic(NOTIONAL_USD, meta_in.decimals);

                if let (Some(router), Some(factory)) = (quickswap_router, quickswap_factory) {
                    if let Some(edge) = quote_v2_edge(
                        &self.provider,
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
                        stats.quotes_succeeded += 1;
                        graph.push(edge);
                    }
                }
                if let (Some(router), Some(factory)) = (sushiswap_router, sushiswap_factory) {
                    if let Some(edge) = quote_v2_edge(
                        &self.provider,
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
                        stats.quotes_succeeded += 1;
                        graph.push(edge);
                    }
                }
                if let (Some(router), Some(factory), Some(quoter)) =
                    (v3_router, v3_factory, v3_quoter)
                {
                    for fee in V3_FEE_TIERS {
                        if let Some(edge) = quote_v3_edge(
                            &self.provider,
                            router,
                            factory,
                            quoter,
                            fee,
                            &meta_in,
                            &meta_out,
                            amount_in,
                            &anchor,
                            &mut pools,
                        )
                        .await
                        {
                            stats.quotes_succeeded += 1;
                            graph.push(edge);
                        }
                    }
                }
            }
        }
        stats.edges_created = graph.edges.len() as u64;

        // ---- Structural cycle discovery: one DFS pass per start token,
        // since route_input is decimal-scaled per starting token.
        // min_hops == max_hops == 3: triangular-only. ----
        let mut raw_routes: Vec<StructuralRoute> = Vec::new();
        for symbol in &symbols {
            let Some(meta) = token_meta.get(symbol) else {
                continue;
            };
            let route_input = human_to_atomic(NOTIONAL_USD, meta.decimals);
            raw_routes.extend(find_structural_cycles(
                &graph,
                &[meta.address],
                3,
                3,
                route_input,
                profile,
            ));
        }
        stats.cycles_detected = raw_routes.len() as u64;
        let mut route_map: BTreeMap<String, StructuralRoute> = BTreeMap::new();
        for route in raw_routes {
            route_map
                .entry(route.structural_cycle_key.clone())
                .or_insert(route);
        }
        stats.routes_discovered = route_map.len() as u64;

        // ---- Phase B: sequential re-quote per structural route so
        // leg[n].amount_in == leg[n-1].amount_out with real adapter output.
        // Caching by (pool, amount_in) keeps real RPC volume bounded when
        // several structural routes share the same first leg. ----
        let mut route_leg_quotes: HashMap<String, Vec<PinnedQuoteRecord>> = HashMap::new();
        let mut requote_cache: HashMap<(Address, U256), PinnedQuoteRecord> = HashMap::new();

        for (key, route) in &route_map {
            let executable_legs = route.executable_legs.clone().unwrap_or_default();
            if executable_legs.is_empty() {
                rejections.push(CanonicalRejection {
                    stage: "leg_quote_chain",
                    reason: "ADAPTER_MISSING_TYPED_LEGS".to_string(),
                    anchor: anchor.clone(),
                    structural_cycle_key: Some(key.clone()),
                });
                continue;
            }

            let mut leg_quotes: Vec<PinnedQuoteRecord> = Vec::new();
            let mut current_amount = route.route_input;
            let mut chain_ok = true;
            for leg in &executable_legs {
                let cache_key = (leg.pool, current_amount);
                let quote = if let Some(cached) = requote_cache.get(&cache_key) {
                    Some(cached.clone())
                } else {
                    let fresh = requote_leg(
                        &self.provider,
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

            let assembled = chain_ok.then(|| {
                assemble_route_leg_quotes(
                    route.route_input,
                    route.anchor_block,
                    route.anchor_block_hash,
                    leg_quotes.clone(),
                    executable_legs.len(),
                )
            });
            match assembled {
                Some(Ok(_)) => {
                    route_leg_quotes.insert(key.clone(), leg_quotes);
                }
                Some(Err(err)) => {
                    rejections.push(CanonicalRejection {
                        stage: "leg_quote_chain",
                        reason: err.to_string(),
                        anchor: anchor.clone(),
                        structural_cycle_key: Some(key.clone()),
                    });
                }
                None => {
                    rejections.push(CanonicalRejection {
                        stage: "leg_quote_chain",
                        reason: "LEG_QUOTE_CHAIN_INCOMPLETE".to_string(),
                        anchor: anchor.clone(),
                        structural_cycle_key: Some(key.clone()),
                    });
                }
            }
        }

        let pool_states: HashMap<Address, SimulatedPoolState> = pools
            .state
            .iter()
            .map(|(addr, s)| (*addr, s.state))
            .collect();

        // ---- Canonical execution context: real typed tokens/pools/states
        // for this anchor, validated and hashed. No persistence, no
        // reload — this service performs no filesystem or fork IO. ----
        if token_meta.is_empty() || pools.meta.is_empty() || pools.state.is_empty() {
            return Ok(CanonicalDiscoveryResult {
                anchor,
                round_evidence,
                executable_routes,
                economically_positive,
                rejections,
                stats,
                structural_routes: route_map,
                leg_quotes: route_leg_quotes,
                pool_states,
            });
        }

        let ctx_tokens: BTreeMap<String, TokenMetadata> = token_meta
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        let ctx_pools: BTreeMap<String, PoolExecutionMetadata> = pools
            .meta
            .iter()
            .map(|(pool, meta)| (format!("{pool:?}"), meta.clone()))
            .collect();
        let ctx_states: BTreeMap<String, PinnedPoolState> = pools
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
                    caller: CANONICAL_CALLER(),
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

        let context = match CanonicalExecutionContext::build(
            anchor.number,
            anchor.hash,
            ctx_tokens,
            ctx_pools,
            ctx_states,
            ctx_setup,
        ) {
            Ok(context) => context,
            Err(err) => {
                rejections.push(CanonicalRejection {
                    stage: "context_build",
                    reason: err.to_string(),
                    anchor: anchor.clone(),
                    structural_cycle_key: None,
                });
                return Ok(CanonicalDiscoveryResult {
                    anchor,
                    round_evidence,
                    executable_routes,
                    economically_positive,
                    rejections,
                    stats,
                    structural_routes: route_map,
                    leg_quotes: route_leg_quotes,
                    pool_states,
                });
            }
        };

        let mut token_records: HashMap<String, TokenRecord> = HashMap::new();
        for (sym, t) in &token_meta {
            token_records.insert(
                sym.clone(),
                TokenRecord {
                    address: t.address,
                    decimals: t.decimals,
                },
            );
        }
        let mut pool_records: HashMap<String, PoolRecord> = HashMap::new();
        for (pool_addr, meta) in &pools.meta {
            let Some(state) = pools.state.get(pool_addr) else {
                continue;
            };
            pool_records.insert(
                format!("{pool_addr:?}"),
                PoolRecord {
                    address: meta.pool,
                    router: meta.router,
                    state: state.state,
                    bytecode_present: true,
                    curve_method: None,
                    token_in_index: None,
                    token_out_index: None,
                },
            );
        }
        let mut venue_records: HashMap<String, VenueRecord> = HashMap::new();
        for meta in pools.meta.values() {
            let venue = match meta.venue.as_str() {
                "QuickSwap" => Venue::QuickSwap,
                "SushiSwap" => Venue::SushiSwap,
                "UniswapV3" => Venue::UniswapV3,
                _ => continue,
            };
            venue_records
                .entry(meta.venue.clone())
                .or_insert(VenueRecord {
                    venue,
                    router: meta.router,
                });
        }

        // ---- Materialization + pure route economics. Fork-audit evidence
        // (real gas, real balance delta, trace validation) is deliberately
        // absent — that belongs to the binary's fork-execution stage. ----
        for (key, route) in &route_map {
            let Some(leg_quotes) = route_leg_quotes.get(key) else {
                continue;
            };
            let plan = match materialize(
                route,
                anchor.number,
                CANONICAL_CALLER(),
                &token_records,
                &pool_records,
                &venue_records,
                false,
                route.route_input,
            ) {
                Ok(plan) => plan,
                Err(err) => {
                    rejections.push(CanonicalRejection {
                        stage: "materialize",
                        reason: format!("{err:?}"),
                        anchor: anchor.clone(),
                        structural_cycle_key: Some(key.clone()),
                    });
                    continue;
                }
            };
            stats.routes_materialized += 1;

            let Some(start_decimals) = token_meta_by_addr
                .get(&plan.start_token)
                .map(|t| t.decimals)
            else {
                rejections.push(CanonicalRejection {
                    stage: "economics",
                    reason: "ECONOMIC_START_TOKEN_METADATA_MISSING".to_string(),
                    anchor: anchor.clone(),
                    structural_cycle_key: Some(key.clone()),
                });
                continue;
            };
            let first_touch_quotes: Vec<U256> = leg_quotes.iter().map(|q| q.amount_out).collect();
            let economics = StatefulRouteEvaluator.evaluate(
                &route.legs,
                route.route_input,
                &plan.snapshot,
                &SimulationContext {
                    start_decimals,
                    gas_cost_atomic: U256::zero(),
                },
                &first_touch_quotes,
            );

            match economics {
                Ok(result) => {
                    stats.economics_evaluated += 1;
                    let evidence = RoundEvidence {
                        structural_cycle_key: key.clone(),
                        anchor: anchor.clone(),
                        context_hash: context.context_hash,
                        route_plan: plan.clone(),
                        amount_in: route.route_input,
                        execution_profile: ExecutionProfile {
                            chain_id: self.expected_chain_id,
                            profile_label: profile.to_string(),
                        },
                        gross_pnl_atomic: Some(result.gross_pnl_atomic),
                        gas_used_total: 0,
                        orchestrator_evidence: None,
                        rejected_registry_hit: false,
                        economics: Some(result.clone()),
                    };
                    let is_positive = result.net_pnl_atomic > 0;
                    round_evidence.push(evidence);
                    executable_routes.push(plan.clone());
                    if is_positive {
                        economically_positive.push(plan);
                    }
                }
                Err(err) => {
                    rejections.push(CanonicalRejection {
                        stage: "economics",
                        reason: err.to_string(),
                        anchor: anchor.clone(),
                        structural_cycle_key: Some(key.clone()),
                    });
                }
            }
        }

        Ok(CanonicalDiscoveryResult {
            anchor,
            round_evidence,
            executable_routes,
            economically_positive,
            rejections,
            stats,
            structural_routes: route_map,
            leg_quotes: route_leg_quotes,
            pool_states,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pinned_anchor_is_core_api() {
        let anchor = PinnedAnchor {
            number: 1,
            hash: H256::repeat_byte(1),
            selected_from_head: 1,
            confirmation_lag: 0,
        };
        assert_ne!(anchor.hash, H256::zero());
    }
}
