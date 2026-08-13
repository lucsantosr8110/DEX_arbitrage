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
use futures::{stream, StreamExt};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Duration,
};
use thiserror::Error;

pub type PinnedAnchor = AnchorBlock;

// ============================================================
// Operational pipeline constants
// ============================================================

const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUOTE_TIMEOUT: Duration = Duration::from_secs(20);
const NOTIONAL_USD: f64 = 100.0;
const V3_FEE_TIERS: [u32; 3] = [500, 3000, 10_000];
const CANONICAL_CALLER: fn() -> Address = || Address::from_low_u64_be(1);

// ============================================================
// Typed discovery-universe configuration. Resolved once, from real
// `Config` data, before any RPC call — `discover_at` never resolves an
// address from a symbol or picks a venue by string match.
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalDiscoveryProfile {
    Base,
    Liquid,
}

impl CanonicalDiscoveryProfile {
    /// Diagnostic/route-id label only — never used to resolve an address.
    pub fn label(self) -> &'static str {
        match self {
            Self::Base => "base",
            Self::Liquid => "liquid",
        }
    }
}

/// `Address` is the executable identity; `symbol` exists for diagnostics
/// and presentation only and is never used to resolve execution state.
#[derive(Debug, Clone)]
pub struct CanonicalToken {
    pub address: Address,
    pub decimals: u8,
    pub symbol: String,
}

#[derive(Debug, Clone)]
pub struct CanonicalVenueConfig {
    pub venue: Venue,
    pub router: Address,
    pub factory: Address,
    /// `Some` only for `Venue::UniswapV3`.
    pub quoter: Option<Address>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CanonicalDiscoveryConfigError {
    #[error("CANONICAL_CONFIG_TOKEN_MISSING_ADDRESS: {0}")]
    MissingAddress(String),
    #[error("CANONICAL_CONFIG_TOKEN_MISSING_DECIMALS: {0}")]
    MissingDecimals(String),
    #[error("CANONICAL_CONFIG_EMPTY_TOKEN_UNIVERSE")]
    EmptyUniverse,
    #[error("CANONICAL_CONFIG_NO_VENUES_RESOLVED")]
    NoVenues,
}

/// Immutable, fully-typed discovery-universe configuration. Built once
/// (typically at process startup) from real `Config` data; `discover_at`
/// only ever reads from this, never from `Config` or a raw symbol again.
#[derive(Debug, Clone)]
pub struct CanonicalDiscoveryConfig {
    pub profile: CanonicalDiscoveryProfile,
    pub tokens: Vec<CanonicalToken>,
    pub venues: Vec<CanonicalVenueConfig>,
    pub execution_profile: ExecutionProfile,
}

impl CanonicalDiscoveryConfig {
    const BASE_TOKENS: &'static [&'static str] = &["USDC", "USDT", "WMATIC", "WETH", "WBTC"];
    // `UNI`/`LDO` are listed as intended midcap tokens elsewhere in
    // `config.toml` (`[arbitrage.triangular].midcaps`) but have no resolved
    // `[pairs.tokens]` address there yet — an existing config gap, not
    // something this service fabricates an address for. `Liquid` stays
    // real and strictly a superset of `Base` using only tokens with a
    // genuine on-chain address already configured.
    const LIQUID_TOKENS: &'static [&'static str] = &[
        "USDC", "USDT", "WMATIC", "WETH", "WBTC", "DAI", "LINK", "AAVE",
    ];
    const KNOWN_VENUES: &'static [(&'static str, Venue)] = &[
        ("QuickSwap", Venue::QuickSwap),
        ("SushiSwap", Venue::SushiSwap),
        ("UniswapV3", Venue::UniswapV3),
    ];

    /// The only place a token symbol is ever resolved to an `Address`. Runs
    /// once against static `Config` data — no RPC, no per-round lookup.
    /// Fails closed: an incomplete/invalid universe never silently falls
    /// back to a smaller or different one.
    pub fn from_config(
        cfg: &Config,
        profile: CanonicalDiscoveryProfile,
        execution_profile: ExecutionProfile,
    ) -> Result<Self, CanonicalDiscoveryConfigError> {
        let default_symbols: &[&str] = match profile {
            CanonicalDiscoveryProfile::Base => Self::BASE_TOKENS,
            CanonicalDiscoveryProfile::Liquid => Self::LIQUID_TOKENS,
        };
        let token_override = std::env::var("CANONICAL_TOKEN_UNIVERSE").ok();
        let symbols: Vec<String> = token_override
            .as_deref()
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|symbol| !symbol.is_empty())
                    .map(str::to_uppercase)
                    .collect()
            })
            .unwrap_or_else(|| default_symbols.iter().map(|s| (*s).to_string()).collect());
        let mut tokens = Vec::with_capacity(symbols.len());
        for symbol in symbols {
            let address = cfg
                .addresses
                .get(&symbol)
                .copied()
                .ok_or_else(|| CanonicalDiscoveryConfigError::MissingAddress(symbol.clone()))?;
            let decimals = cfg
                .pairs
                .metadata
                .get(&symbol)
                .and_then(|m| m.decimals)
                .ok_or_else(|| CanonicalDiscoveryConfigError::MissingDecimals(symbol.clone()))?;
            tokens.push(CanonicalToken {
                address,
                decimals,
                symbol,
            });
        }
        if tokens.is_empty() {
            return Err(CanonicalDiscoveryConfigError::EmptyUniverse);
        }

        let venue_override = std::env::var("CANONICAL_VENUES").ok().map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_ascii_lowercase)
                .collect::<std::collections::HashSet<_>>()
        });
        let mut venues = Vec::new();
        for (name, venue) in Self::KNOWN_VENUES.iter().copied() {
            if let Some(allowed) = &venue_override {
                if !allowed.contains(&name.to_ascii_lowercase()) {
                    continue;
                }
            }
            let Some(dex) = cfg.dex.iter().find(|d| d.name == name) else {
                continue;
            };
            let Ok(router) = dex.router_address.parse::<Address>() else {
                continue;
            };
            let Some(factory) = dex
                .factory_address
                .clone()
                .and_then(|s| s.parse::<Address>().ok())
            else {
                continue;
            };
            let quoter = (venue == Venue::UniswapV3).then(|| {
                dex.quoter_address
                    .clone()
                    .and_then(|s| s.parse::<Address>().ok())
                    .or_else(|| UNISWAP_V3_QUOTER.parse::<Address>().ok())
            });
            venues.push(CanonicalVenueConfig {
                venue,
                router,
                factory,
                quoter: quoter.flatten(),
            });
        }
        if venues.is_empty() {
            return Err(CanonicalDiscoveryConfigError::NoVenues);
        }

        Ok(Self {
            profile,
            tokens,
            venues,
            execution_profile,
        })
    }

    pub fn token_count(&self) -> usize {
        self.tokens.len()
    }

    /// Deterministic diagnostic fingerprint of the resolved token universe.
    /// Safe to print/persist: it is a hash, never a raw RPC endpoint.
    pub fn token_addresses_hash(&self) -> H256 {
        let mut addresses: Vec<Address> = self.tokens.iter().map(|t| t.address).collect();
        addresses.sort();
        H256::from(ethers::utils::keccak256(
            format!("{addresses:?}").as_bytes(),
        ))
    }

    fn venue_config(&self, venue: Venue) -> Option<&CanonicalVenueConfig> {
        self.venues.iter().find(|v| v.venue == venue)
    }
}

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
    pub routes_pruned: u64,
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
    /// All successful single-leg quotes from Phase A, retained for
    /// presentation consumers even when route pruning limits Phase B.
    pub initial_quotes: Vec<PinnedQuoteRecord>,
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
    config: CanonicalDiscoveryConfig,
}

impl<M> CanonicalDiscoveryService<M>
where
    M: Middleware,
    M::Error: 'static,
{
    pub fn new(provider: Arc<M>, expected_chain_id: u64, config: CanonicalDiscoveryConfig) -> Self {
        Self {
            provider,
            expected_chain_id,
            config,
        }
    }

    pub fn config(&self) -> &CanonicalDiscoveryConfig {
        &self.config
    }

    pub async fn discover_at(&self, anchor: PinnedAnchor) -> Result<CanonicalDiscoveryResult> {
        let timeout_secs = std::env::var("CANONICAL_DISCOVERY_TIMEOUT_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(900);
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            timeout_secs,
            "canonical discovery started"
        );
        match tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            self.discover_at_inner(anchor),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(anyhow!("CANONICAL_DISCOVERY_TIMEOUT after {timeout_secs}s")),
        }
    }

    async fn discover_at_inner(&self, anchor: PinnedAnchor) -> Result<CanonicalDiscoveryResult> {
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

        let profile = self.config.profile.label();

        let mut stats = DiscoveryStats::default();
        let mut rejections: Vec<CanonicalRejection> = Vec::new();
        let mut round_evidence: Vec<RoundEvidence> = Vec::new();
        let mut executable_routes = Vec::new();
        let mut economically_positive = Vec::new();
        let mut initial_quotes: Vec<PinnedQuoteRecord> = Vec::new();

        // ---- Pool/token metadata: real on-chain code hash, pinned to the
        // anchor block. Address/decimals/symbol are already resolved and
        // typed in `self.config.tokens` — no symbol/address lookup happens
        // here or anywhere else in this method. ----
        let mut token_meta: HashMap<String, TokenMetadata> = HashMap::new();
        for token in &self.config.tokens {
            let Ok(code) = self
                .provider
                .get_code(
                    token.address,
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
                token.symbol.clone(),
                TokenMetadata {
                    address: token.address,
                    symbol: token.symbol.clone(),
                    decimals: token.decimals,
                    code_hash: hash,
                    anchor_block: anchor.number,
                },
            );
        }
        let token_meta_by_addr: HashMap<Address, TokenMetadata> = token_meta
            .values()
            .map(|t| (t.address, t.clone()))
            .collect();
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            tokens_configured = self.config.tokens.len(),
            tokens_with_code = token_meta.len(),
            "canonical metadata stage complete"
        );
        let symbols: Vec<String> = self
            .config
            .tokens
            .iter()
            .map(|t| t.symbol.clone())
            .collect();

        let quickswap = self.config.venue_config(Venue::QuickSwap);
        let sushiswap = self.config.venue_config(Venue::SushiSwap);
        let v3 = self.config.venue_config(Venue::UniswapV3);

        // ---- Phase A: independent single-leg quotes -> typed edges.
        // Curve is intentionally never quoted here. ----
        let mut graph = ExecutableEdgeGraph::new();
        let mut pools = PoolContext::default();

        let quote_concurrency = std::env::var("CANONICAL_QUOTE_CONCURRENCY")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(4);
        let mut pair_inputs = Vec::new();
        for symbol_in in &symbols {
            for symbol_out in &symbols {
                if symbol_in == symbol_out {
                    continue;
                }
                let (Some(meta_in), Some(meta_out)) =
                    (token_meta.get(symbol_in), token_meta.get(symbol_out))
                else {
                    continue;
                };
                pair_inputs.push((
                    symbol_in.clone(),
                    symbol_out.clone(),
                    meta_in.clone(),
                    meta_out.clone(),
                ));
            }
        }
        let pair_inputs: Vec<_> = pair_inputs.into_iter().enumerate().collect();
        stats.quotes_attempted = pair_inputs.len() as u64;
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            pairs = pair_inputs.len(),
            quote_concurrency,
            "canonical quote stage started"
        );

        let provider = self.provider.clone();
        let anchor_for_quotes = anchor.clone();
        let quote_results = stream::iter(pair_inputs.into_iter().map(
            |(pair_index, (symbol_in, symbol_out, meta_in, meta_out))| {
                let provider = provider.clone();
                let anchor = anchor_for_quotes.clone();
                let quickswap = quickswap.cloned();
                let sushiswap = sushiswap.cloned();
                let v3 = v3.cloned();
                async move {
                    tracing::info!(
                        target: "canonical_discovery",
                        anchor = anchor.number,
                        token_in = %symbol_in,
                        token_out = %symbol_out,
                        "canonical quote pair started"
                    );
                    let mut local_pools = PoolContext::default();
                    let mut edges = Vec::new();
                    let amount_in = human_to_atomic(NOTIONAL_USD, meta_in.decimals);
                    if let Some(cfg) = quickswap {
                        if let Some(edge) = quote_v2_edge(
                            &provider,
                            Venue::QuickSwap,
                            cfg.router,
                            cfg.factory,
                            &meta_in,
                            &meta_out,
                            amount_in,
                            &anchor,
                            &mut local_pools,
                        )
                        .await
                        {
                            edges.push(edge);
                        }
                    }
                    if let Some(cfg) = sushiswap {
                        if let Some(edge) = quote_v2_edge(
                            &provider,
                            Venue::SushiSwap,
                            cfg.router,
                            cfg.factory,
                            &meta_in,
                            &meta_out,
                            amount_in,
                            &anchor,
                            &mut local_pools,
                        )
                        .await
                        {
                            edges.push(edge);
                        }
                    }
                    if let Some(cfg) = v3.filter(|cfg| cfg.quoter.is_some()) {
                        let quoter = cfg.quoter.expect("filtered on Some");
                        for fee in V3_FEE_TIERS {
                            if let Some(edge) = quote_v3_edge(
                                &provider,
                                cfg.router,
                                cfg.factory,
                                quoter,
                                fee,
                                &meta_in,
                                &meta_out,
                                amount_in,
                                &anchor,
                                &mut local_pools,
                            )
                            .await
                            {
                                edges.push(edge);
                            }
                        }
                    }
                    tracing::info!(
                        target: "canonical_discovery",
                        anchor = anchor.number,
                        token_in = %symbol_in,
                        token_out = %symbol_out,
                        quotes_succeeded = edges.len(),
                        "canonical quote pair complete"
                    );
                    (pair_index, edges, local_pools)
                }
            },
        ))
        .buffer_unordered(quote_concurrency)
        .collect::<Vec<_>>()
        .await;

        let mut quote_results = quote_results;
        quote_results.sort_by_key(|(pair_index, _, _)| *pair_index);
        for (_, edges, local_pools) in quote_results {
            stats.quotes_succeeded += edges.len() as u64;
            for edge in edges {
                initial_quotes.push(PinnedQuoteRecord {
                    quote_id: edge.quote_id,
                    anchor_block: edge.anchor_block,
                    anchor_hash: edge.anchor_block_hash,
                    venue: edge.venue,
                    pool: edge.pool,
                    token_in: edge.token_in,
                    token_out: edge.token_out,
                    amount_in: edge.amount_in,
                    amount_out: edge.amount_out,
                    pool_state_id: edge.pool_state_id,
                    execution_metadata_id: edge.execution_metadata_id,
                    adapter_version: "canonical-edge".into(),
                    provenance_hash: edge.provenance_hash,
                });
                graph.push(edge);
            }
            pools.meta.extend(local_pools.meta);
            pools.state.extend(local_pools.state);
            pools.quote_target.extend(local_pools.quote_target);
        }
        stats.edges_created = graph.edges.len() as u64;
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            quotes_attempted = stats.quotes_attempted,
            quotes_succeeded = stats.quotes_succeeded,
            edges_created = stats.edges_created,
            pools_observed = pools.meta.len(),
            "canonical quote stage complete"
        );

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
        let max_routes = std::env::var("CANONICAL_MAX_ROUTES_PER_ROUND")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(128);
        let discovered_routes = route_map.len();
        if discovered_routes > max_routes {
            route_map = route_map.into_iter().take(max_routes).collect();
        }
        stats.routes_discovered = route_map.len() as u64;
        stats.routes_pruned = (discovered_routes - route_map.len()) as u64;
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            cycles_detected = stats.cycles_detected,
            routes_discovered = stats.routes_discovered,
            routes_pruned = stats.routes_pruned,
            max_routes,
            "canonical structural stage complete"
        );

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
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            routes_discovered = route_map.len(),
            routes_requoted = route_leg_quotes.len(),
            rejections = rejections.len(),
            "canonical sequential requote stage complete"
        );

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
                initial_quotes,
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

        let context_pools = ctx_pools.len();
        let context_states = ctx_states.len();
        let context_routes = ctx_setup.len();
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
                    initial_quotes,
                    structural_routes: route_map,
                    leg_quotes: route_leg_quotes,
                    pool_states,
                });
            }
        };
        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            context_pools,
            context_states,
            context_routes,
            "canonical execution context stage complete"
        );

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
                        execution_profile: self.config.execution_profile.clone(),
                        gross_pnl_atomic: Some(result.gross_pnl_atomic),
                        gas_used_total: 0,
                        orchestrator_evidence: None,
                        rejected_registry_hit: false,
                        economics: Some(result.clone()),
                        leg_quotes: leg_quotes.clone(),
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

        tracing::info!(
            target: "canonical_discovery",
            anchor = anchor.number,
            routes_materialized = stats.routes_materialized,
            economics_evaluated = stats.economics_evaluated,
            executable_routes = executable_routes.len(),
            economically_positive = economically_positive.len(),
            rejections = rejections.len(),
            "canonical discovery complete"
        );

        Ok(CanonicalDiscoveryResult {
            anchor,
            round_evidence,
            executable_routes,
            economically_positive,
            rejections,
            stats,
            initial_quotes,
            structural_routes: route_map,
            leg_quotes: route_leg_quotes,
            pool_states,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::DexEntry;

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

    fn a(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn exec_profile() -> ExecutionProfile {
        ExecutionProfile {
            chain_id: 137,
            profile_label: "test".into(),
        }
    }

    /// Builds a config with real (distinct) addresses for every
    /// `Base`/`Liquid` symbol plus one resolvable venue, so `from_config`
    /// can succeed for both profiles without touching disk.
    fn config_fixture() -> Config {
        let mut cfg = Config::default();
        let symbols = [
            "USDC", "USDT", "WMATIC", "WETH", "WBTC", "DAI", "LINK", "AAVE",
        ];
        for (i, symbol) in symbols.iter().enumerate() {
            cfg.addresses
                .insert((*symbol).to_string(), a(100 + i as u64));
            cfg.pairs.metadata.insert(
                (*symbol).to_string(),
                crate::config::TokenMetadata {
                    symbol: (*symbol).to_string(),
                    name: None,
                    decimals: Some(18),
                    coingecko_id: None,
                    category: None,
                },
            );
        }
        cfg.dex.push(DexEntry {
            name: "QuickSwap".into(),
            router_address: format!("{:#x}", a(1)),
            factory_address: Some(format!("{:#x}", a(2))),
            enabled: true,
            ..Default::default()
        });
        cfg
    }

    #[test]
    fn base_profile_builds_base_universe() {
        let cfg = config_fixture();
        let config = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Base,
            exec_profile(),
        )
        .unwrap();
        assert_eq!(config.token_count(), 5);
        let symbols: std::collections::BTreeSet<_> =
            config.tokens.iter().map(|t| t.symbol.as_str()).collect();
        assert_eq!(
            symbols,
            ["USDC", "USDT", "WMATIC", "WETH", "WBTC"]
                .into_iter()
                .collect()
        );
    }

    #[test]
    fn liquid_profile_builds_liquid_universe() {
        let cfg = config_fixture();
        let config = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Liquid,
            exec_profile(),
        )
        .unwrap();
        assert_eq!(config.token_count(), 8);
    }

    #[test]
    fn base_and_liquid_profiles_are_not_cosmetic() {
        let cfg = config_fixture();
        let base = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Base,
            exec_profile(),
        )
        .unwrap();
        let liquid = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Liquid,
            exec_profile(),
        )
        .unwrap();
        // Materially different token counts and address sets, not just a
        // different label on the same underlying universe.
        assert_ne!(base.token_count(), liquid.token_count());
        assert_ne!(base.token_addresses_hash(), liquid.token_addresses_hash());
        let base_addrs: std::collections::BTreeSet<_> =
            base.tokens.iter().map(|t| t.address).collect();
        let liquid_addrs: std::collections::BTreeSet<_> =
            liquid.tokens.iter().map(|t| t.address).collect();
        assert!(liquid_addrs.is_superset(&base_addrs));
        assert!(liquid_addrs.len() > base_addrs.len());
    }

    #[test]
    fn base_profile_is_not_hardcoded() {
        // Same symbol, different configured address -> the resolved
        // CanonicalToken follows the real config, proving the universe is
        // read from `Config`, not compiled in as a fixed address.
        let mut cfg = config_fixture();
        cfg.addresses.insert("USDC".to_string(), a(999));
        let config = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Base,
            exec_profile(),
        )
        .unwrap();
        let usdc = config.tokens.iter().find(|t| t.symbol == "USDC").unwrap();
        assert_eq!(usdc.address, a(999));
    }

    #[test]
    fn discover_at_does_not_fallback_to_base_on_incomplete_liquid_config() {
        let mut cfg = config_fixture();
        cfg.addresses.remove("LINK");
        let result = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Liquid,
            exec_profile(),
        );
        assert_eq!(
            result.unwrap_err(),
            CanonicalDiscoveryConfigError::MissingAddress("LINK".to_string())
        );
    }

    #[test]
    fn empty_dex_config_fails_closed_with_no_silent_venue_fallback() {
        let mut cfg = config_fixture();
        cfg.dex.clear();
        let result = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Base,
            exec_profile(),
        );
        assert_eq!(result.unwrap_err(), CanonicalDiscoveryConfigError::NoVenues);
    }

    #[test]
    fn token_symbol_is_not_execution_identity() {
        // `symbol` is presentation-only: the resolved `Address` — not the
        // string — is what a caller must use to reason about identity.
        let cfg = config_fixture();
        let config = CanonicalDiscoveryConfig::from_config(
            &cfg,
            CanonicalDiscoveryProfile::Base,
            exec_profile(),
        )
        .unwrap();
        let mut poisoned = config.clone();
        for token in &mut poisoned.tokens {
            token.symbol = "NOT_A_REAL_SYMBOL".into();
        }
        // Corrupting every symbol string leaves the address-derived
        // fingerprint (the only thing execution can key off of) unchanged.
        assert_eq!(
            config.token_addresses_hash(),
            poisoned.token_addresses_hash()
        );
    }
}
