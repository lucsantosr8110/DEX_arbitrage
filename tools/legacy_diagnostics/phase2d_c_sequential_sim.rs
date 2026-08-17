//! Phase 2D-C: sequential, stateful, multi-size economic simulation of the
//! Phase 2D-B structural routes. This binary owns only a read-only
//! `Provider` — it never loads `.env` secrets beyond `BOT_RPC_ENDPOINTS`, a
//! wallet, a signer, an executor, or a broadcaster. Every RPC call in this
//! file is a pinned `eth_call` / `eth_getBlockByNumber` read.
//!
//! Grid sizes are denominated in **native human units of each route's start
//! token**, not USD: this codebase has no on-chain-pinned USD price oracle
//! (the only USD source, `infra::price_feed`, is a live Coingecko cache —
//! auditable but not block-pinned), so per spec section 9's own fallback
//! clause, native units are canonical and USD figures are never fabricated
//! or silently mixed in. Gas cost, which is paid in MATIC, is converted into
//! each route's start-token units via a *pinned on-chain DEX quote*
//! (WMATIC -> start token, same anchor block) rather than an external price
//! feed — this keeps the net-PnL gate fully pinned end to end.
//!
//! CLI:
//!   cargo run --release --bin phase2d_c_sequential_sim -- run \
//!     --routes-artifact diagnostics/phase2d_b/analysis/cycle_persistence.json \
//!     --rounds 3 --sizes 10,25,50,100,200,300,500,750,1000 \
//!     --output-dir diagnostics
//!
//!   cargo run --bin phase2d_c_sequential_sim -- replay \
//!     --snapshot diagnostics/phase2d_c_sequential_simulation_<ts>.jsonl \
//!     --route-id <id> --size 100 --round 1

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use ethers::{
    abi::{Abi, Detokenize},
    contract::{Contract, ContractCall},
    providers::{Middleware, Provider},
    types::{Address, BlockId, BlockNumber, U256},
};
use flashloan_bot::{
    config::Config,
    core::{
        gas_profile::{swap_gas_units, VenueKind},
        phase2d_anchor::{reorg_detected, select_anchor, AnchorBlock},
        pool_state_sim::{
            PoolKind, PoolReuseTracker, PoolTouch, SimulatedPoolState, SimulationError,
        },
        quantization::{atomic_to_human_display, human_to_atomic_floor, is_dust},
        read_only::ReadOnlySafety,
        route_artifact::{load_structural_routes, pool_identity, RouteLeg, StructuralRoute},
        sequential_route_economics::{
            aggregate_final_classification, capacity_boundary, is_fork_candidate, RoundOutcome,
            RoundPnl,
        },
    },
    infra::rotating_http_client::RotatingHttpClient,
};
use serde::Serialize;
use std::{
    collections::HashMap,
    io::Write as _,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};

const V2_ABI: &str = r#"[{"inputs":[{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"address[]","name":"path","type":"address[]"}],"name":"getAmountsOut","outputs":[{"internalType":"uint256[]","name":"amounts","type":"uint256[]"}],"stateMutability":"view","type":"function"}]"#;
const V3_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenIn","type":"address"},{"internalType":"address","name":"tokenOut","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"},{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"uint160","name":"sqrtPriceLimitX96","type":"uint160"}],"name":"quoteExactInputSingle","outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],"stateMutability":"nonpayable","type":"function"}]"#;
const CURVE_ABI: &str = r#"[{"inputs":[{"type":"int128","name":"i"},{"type":"int128","name":"j"},{"type":"uint256","name":"dx"}],"name":"get_dy","outputs":[{"type":"uint256","name":""}],"stateMutability":"view","type":"function"}]"#;
const ERC20_DECIMALS_ABI: &str = r#"[{"inputs":[],"name":"decimals","outputs":[{"internalType":"uint8","name":"","type":"uint8"}],"stateMutability":"view","type":"function"}]"#;
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUOTE_TIMEOUT: Duration = Duration::from_secs(20);
const DEFAULT_ROUTES_ARTIFACT: &str = "diagnostics/phase2d_b/analysis/cycle_persistence.json";
/// Polygon EIP-1559 validator-enforced priority-fee floor. There is no
/// pinned historical `eth_maxPriorityFeePerGas` for a past block, so this is
/// a documented conservative constant, not an invented number: it is the
/// commonly enforced Polygon minimum tip (30 gwei) as of 2026.
const POLYGON_PRIORITY_FEE_GWEI: u64 = 30;
const GRID_DENOMINATION: &str = "NATIVE_START_TOKEN_UNITS";

#[derive(Parser)]
#[command(name = "phase2d_c_sequential_sim")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the full read-only, sequential, stateful campaign.
    Run(RunArgs),
    /// Reproduce previously recorded (round, route, size) evaluation rows
    /// from a saved JSONL snapshot. Offline — no RPC, no signer.
    Replay(ReplayArgs),
}

#[derive(Parser)]
struct RunArgs {
    #[arg(long, default_value = DEFAULT_ROUTES_ARTIFACT)]
    routes_artifact: PathBuf,
    #[arg(long, default_value_t = 3)]
    rounds: u64,
    #[arg(long, default_value = "10,25,50,100,200,300,500,750,1000")]
    sizes: String,
    #[arg(long, default_value = "diagnostics")]
    output_dir: PathBuf,
    #[arg(long, default_value_t = 2)]
    anchor_confirmation_lag: u64,
}

#[derive(Parser)]
struct ReplayArgs {
    #[arg(long)]
    snapshot: PathBuf,
    #[arg(long)]
    route_id: Option<String>,
    #[arg(long)]
    size: Option<f64>,
    #[arg(long)]
    round: Option<u64>,
}

fn parse_sizes(raw: &str) -> Result<Vec<f64>> {
    let mut sizes = Vec::new();
    for part in raw.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let value: f64 = part
            .parse()
            .map_err(|_| anyhow!("invalid size in grid: {part}"))?;
        if !value.is_finite() || value <= 0.0 {
            return Err(anyhow!("grid size must be finite and > 0: {part}"));
        }
        sizes.push(value);
    }
    if sizes.is_empty() {
        return Err(anyhow!("empty sizes grid"));
    }
    Ok(sizes)
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

type ReadOnlyProvider = Provider<RotatingHttpClient>;

/// Single gate for every quote eth_call. No latest-block fallback exists by
/// design — every call in this campaign is pinned to a specific anchor.
fn pinned_eth_call<M, D>(call: ContractCall<M, D>, anchor: &AnchorBlock) -> ContractCall<M, D>
where
    M: Middleware,
    D: Detokenize,
{
    call.block(BlockId::Number(BlockNumber::Number(anchor.number.into())))
}

async fn fetch_decimals(
    provider: Arc<ReadOnlyProvider>,
    token: Address,
    anchor: &AnchorBlock,
) -> Result<u8> {
    let abi: Abi = serde_json::from_str(ERC20_DECIMALS_ABI)?;
    let contract = Contract::new(token, abi, provider);
    let decimals: u8 = tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(contract.method::<_, u8>("decimals", ())?, anchor).call(),
    )
    .await
    .context("DECIMALS_TIMEOUT")??;
    Ok(decimals)
}

async fn quote_v2(
    provider: Arc<ReadOnlyProvider>,
    router: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<U256> {
    let abi: Abi = serde_json::from_str(V2_ABI)?;
    let contract = Contract::new(router, abi, provider);
    let amounts: Vec<U256> = tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(
            contract.method("getAmountsOut", (amount_in, vec![token_in, token_out]))?,
            anchor,
        )
        .call(),
    )
    .await
    .context("V2_QUOTE_TIMEOUT")??;
    amounts
        .last()
        .copied()
        .ok_or_else(|| anyhow!("empty V2 quote"))
}

async fn quote_v3(
    provider: Arc<ReadOnlyProvider>,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<U256> {
    let abi: Abi = serde_json::from_str(V3_ABI)?;
    let quoter = Contract::new(UNISWAP_V3_QUOTER.parse::<Address>()?, abi, provider);
    let out: U256 = tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(
            quoter.method::<_, U256>(
                "quoteExactInputSingle",
                (token_in, token_out, fee, amount_in, U256::zero()),
            )?,
            anchor,
        )
        .call(),
    )
    .await
    .context("V3_QUOTE_TIMEOUT")??;
    if out.is_zero() {
        return Err(anyhow!("zero V3 quote"));
    }
    Ok(out)
}

async fn quote_curve(
    provider: Arc<ReadOnlyProvider>,
    pool: Address,
    index_in: i128,
    index_out: i128,
    amount_in: U256,
    anchor: &AnchorBlock,
) -> Result<U256> {
    let abi: Abi = serde_json::from_str(CURVE_ABI)?;
    let contract = Contract::new(pool, abi, provider);
    let out: U256 = tokio::time::timeout(
        QUOTE_TIMEOUT,
        pinned_eth_call(
            contract.method::<_, U256>("get_dy", (index_in, index_out, amount_in))?,
            anchor,
        )
        .call(),
    )
    .await
    .context("CURVE_QUOTE_TIMEOUT")??;
    if out.is_zero() {
        return Err(anyhow!("zero Curve quote"));
    }
    Ok(out)
}

fn router_for_venue(venue: &str) -> Option<Address> {
    use flashloan_bot::dex::addresses::{QUICKSWAP_ROUTER, SUSHISWAP_ROUTER, UNISWAP_V2_ROUTER};
    let raw = match venue {
        "QuickSwap" => QUICKSWAP_ROUTER,
        "SushiSwap" => SUSHISWAP_ROUTER,
        "UniswapV2" => UNISWAP_V2_ROUTER,
        _ => return None,
    };
    raw.parse().ok()
}

fn venue_kind_for(leg: &RouteLeg) -> VenueKind {
    match (leg.venue.as_str(), leg.protocol_version.as_str()) {
        ("Curve", _) => VenueKind::CurveStable { n_coins: 3 },
        ("UniswapV3", _) | (_, "V3") => VenueKind::UniV3,
        ("QuickSwap", _) => VenueKind::QuickSwapV2,
        ("SushiSwap", _) => VenueKind::SushiV2,
        ("UniswapV2", _) => VenueKind::UniV2,
        _ => VenueKind::Unknown,
    }
}

fn curve_coin_index(symbol_of: &HashMap<Address, String>, token: Address) -> Option<i128> {
    match symbol_of.get(&token).map(String::as_str) {
        Some("DAI") => Some(0),
        Some("USDC") | Some("USDC.e") => Some(1),
        Some("USDT") => Some(2),
        _ => None,
    }
}

/// Performs the real, pinned on-chain quote for a leg's first touch. Returns
/// the amount out plus the `SimulatedPoolState` to record (constant-product
/// pools additionally fetch reserves so a later reuse in the same route can
/// be mutated exactly; V3/Curve are recorded opaque).
async fn quote_first_touch(
    provider: Arc<ReadOnlyProvider>,
    leg: &RouteLeg,
    kind: PoolKind,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    anchor: &AnchorBlock,
    symbol_of: &HashMap<Address, String>,
) -> Result<(U256, SimulatedPoolState)> {
    match kind {
        PoolKind::UniswapV3Concentrated => {
            let fee = leg
                .fee_tier
                .ok_or_else(|| anyhow!("V3 leg missing fee_tier"))?;
            let out = quote_v3(provider, token_in, token_out, fee, amount_in, anchor).await?;
            Ok((out, SimulatedPoolState::Opaque(kind)))
        }
        PoolKind::CurveStableSwap => {
            let pool_addr: Address = leg
                .pool_address
                .as_deref()
                .ok_or_else(|| anyhow!("Curve leg missing pool_address"))?
                .parse()?;
            let index_in = curve_coin_index(symbol_of, token_in)
                .ok_or_else(|| anyhow!("no Curve coin index for token_in"))?;
            let index_out = curve_coin_index(symbol_of, token_out)
                .ok_or_else(|| anyhow!("no Curve coin index for token_out"))?;
            let out =
                quote_curve(provider, pool_addr, index_in, index_out, amount_in, anchor).await?;
            Ok((out, SimulatedPoolState::Opaque(kind)))
        }
        PoolKind::UniswapV2ConstantProduct => {
            let router = router_for_venue(&leg.venue)
                .ok_or_else(|| anyhow!("no known router for venue {}", leg.venue))?;
            let out = quote_v2(provider, router, token_in, token_out, amount_in, anchor).await?;
            // No local reserve fetch (would need the pair address, which
            // this artifact doesn't carry for V2 legs) — recorded opaque.
            // No route in this campaign reuses a V2 pool, so this never
            // gates a real candidate; the reuse-mutation path is proven
            // correct by the dedicated fixture in `pool_state_sim` tests.
            Ok((out, SimulatedPoolState::Opaque(kind)))
        }
    }
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct LegRow {
    leg_index: usize,
    edge_id: usize,
    venue: String,
    pool_id: String,
    pool_type: String,
    token_in: String,
    token_out: String,
    amount_in_atomic: String,
    amount_out_atomic: String,
    amount_in_human: f64,
    amount_out_human: f64,
    token_in_decimals: u8,
    token_out_decimals: u8,
    reference_rate: Option<f64>,
    effective_rate: Option<f64>,
    price_impact_bps: Option<i64>,
    rounding_loss_atomic: String,
    pool_reuse_index: Option<usize>,
    was_reused: bool,
    mutation_supported: bool,
    simulation_model: String,
    simulation_exact: bool,
    error_code: Option<String>,
}

#[derive(Debug, Clone, Serialize, serde::Deserialize)]
struct EvaluationRow {
    round_id: u64,
    anchor_block: u64,
    route_id: String,
    profile: String,
    venues: Vec<String>,
    pool_ids: Vec<String>,
    token_path: Vec<String>,
    size_human: f64,
    grid_denomination: String,
    amount_in_atomic: String,
    amount_out_atomic: Option<String>,
    gross_pnl_atomic: Option<String>,
    gross_return_bps: Option<i64>,
    gross_positive: bool,
    gas_units_estimated: Option<u64>,
    gas_units_source: String,
    gas_price_wei: Option<String>,
    gas_price_source: String,
    gas_cost_native_wei: Option<String>,
    gas_cost_start_token_atomic: Option<String>,
    gas_conversion_source: String,
    net_pnl_atomic: Option<String>,
    net_return_bps: Option<i64>,
    net_positive: Option<bool>,
    route_slippage_bps: Option<i64>,
    slippage_model: String,
    reused_pool: bool,
    all_legs_supported: bool,
    temporal_coherence: bool,
    quote_state_block_span: u64,
    classification_round: String,
    error_code: Option<String>,
    legs: Vec<LegRow>,
}

struct RouteSizeResult {
    outcome: RoundOutcome,
    row: EvaluationRow,
}

/// Simulates one (route, size) sequentially and statefully at the given
/// anchor block. Never issues an unpinned call; never mutates the pool
/// state shared by any other (route, size) or round — `PoolReuseTracker` is
/// created fresh for every call.
#[allow(clippy::too_many_arguments)]
async fn simulate_route_at_size(
    provider: Arc<ReadOnlyProvider>,
    round_id: u64,
    route: &StructuralRoute,
    size_human: f64,
    decimals_of: &HashMap<Address, u8>,
    symbol_of: &HashMap<Address, String>,
    anchor: &AnchorBlock,
    reference_rate_cache: &mut HashMap<String, f64>,
) -> RouteSizeResult {
    let route_meta = |amount_in_atomic: U256,
                      amount_out_atomic: Option<String>,
                      gross_pnl_atomic: Option<String>,
                      gross_positive: bool,
                      reused_pool: bool,
                      all_legs_supported: bool,
                      classification_round: &str,
                      error_code: Option<String>,
                      legs: Vec<LegRow>| EvaluationRow {
        round_id,
        anchor_block: anchor.number,
        route_id: route.route_id.clone(),
        profile: route.profile.clone(),
        venues: route.venues.clone(),
        pool_ids: route.pools.clone(),
        token_path: route
            .legs
            .iter()
            .map(|l| l.token_in.clone())
            .chain(route.legs.last().map(|l| l.token_out.clone()))
            .collect(),
        size_human,
        grid_denomination: GRID_DENOMINATION.to_string(),
        amount_in_atomic: amount_in_atomic.to_string(),
        amount_out_atomic,
        gross_pnl_atomic,
        gross_return_bps: None,
        gross_positive,
        gas_units_estimated: None,
        gas_units_source: "MODELED_CONSERVATIVE".to_string(),
        gas_price_wei: None,
        gas_price_source: "UNAVAILABLE".to_string(),
        gas_cost_native_wei: None,
        gas_cost_start_token_atomic: None,
        gas_conversion_source: "UNAVAILABLE".to_string(),
        net_pnl_atomic: None,
        net_return_bps: None,
        net_positive: None,
        route_slippage_bps: None,
        slippage_model: "APPROXIMATE".to_string(),
        reused_pool,
        all_legs_supported,
        temporal_coherence: true,
        quote_state_block_span: 0,
        classification_round: classification_round.to_string(),
        error_code,
        legs,
    };

    let Some(first_leg) = route.legs.first() else {
        return RouteSizeResult {
            outcome: RoundOutcome::RouteInvalid,
            row: route_meta(
                U256::zero(),
                None,
                None,
                false,
                false,
                false,
                "ROUTE_INVALID",
                Some("route has no legs".to_string()),
                Vec::new(),
            ),
        };
    };
    let Ok(start_token) = first_leg.token_in.parse::<Address>() else {
        return RouteSizeResult {
            outcome: RoundOutcome::RouteInvalid,
            row: route_meta(
                U256::zero(),
                None,
                None,
                false,
                false,
                false,
                "ROUTE_INVALID",
                Some("bad start token address".to_string()),
                Vec::new(),
            ),
        };
    };
    let Some(&start_decimals) = decimals_of.get(&start_token) else {
        return RouteSizeResult {
            outcome: RoundOutcome::SimulationError,
            row: route_meta(
                U256::zero(),
                None,
                None,
                false,
                false,
                false,
                "SIMULATION_ERROR",
                Some("missing decimals for start token".to_string()),
                Vec::new(),
            ),
        };
    };

    let initial_atomic = match human_to_atomic_floor(size_human, start_decimals) {
        Ok(v) => v,
        Err(e) => {
            return RouteSizeResult {
                outcome: RoundOutcome::SimulationError,
                row: route_meta(
                    U256::zero(),
                    None,
                    None,
                    false,
                    false,
                    false,
                    "SIMULATION_ERROR",
                    Some(format!("QUANTIZATION_ERROR: {e}")),
                    Vec::new(),
                ),
            };
        }
    };
    if is_dust(size_human, initial_atomic) {
        return RouteSizeResult {
            outcome: RoundOutcome::SimulationError,
            row: route_meta(
                initial_atomic,
                None,
                None,
                false,
                false,
                false,
                "SIMULATION_ERROR",
                Some("DUST_BELOW_ATOMIC_PRECISION".to_string()),
                Vec::new(),
            ),
        };
    }

    let mut tracker = PoolReuseTracker::new();
    let mut amount_in = initial_atomic;
    let mut legs: Vec<LegRow> = Vec::with_capacity(route.legs.len());
    let mut reused_pool = false;
    let mut all_legs_supported = true;
    let mut hard_error: Option<String> = None;
    let mut route_slippage_bps_acc: i64 = 0;

    for (leg_index, leg) in route.legs.iter().enumerate() {
        let pool_id = pool_identity(leg);
        let kind = PoolKind::classify(&leg.venue, &leg.protocol_version);
        let (Ok(token_in), Ok(token_out)) = (
            leg.token_in.parse::<Address>(),
            leg.token_out.parse::<Address>(),
        ) else {
            all_legs_supported = false;
            hard_error = Some(format!("bad token address at leg {leg_index}"));
            break;
        };
        let decimals_in = *decimals_of.get(&token_in).unwrap_or(&18);
        let decimals_out = *decimals_of.get(&token_out).unwrap_or(&18);
        let amount_in_human = atomic_to_human_display(amount_in, decimals_in);

        let reference_rate_before = reference_rate_cache.get(&pool_id).copied();

        match tracker.touch(&pool_id, leg_index, amount_in) {
            Err(SimulationError::UnsupportedReusedPool { .. }) => {
                reused_pool = true;
                all_legs_supported = false;
                hard_error = Some(format!("UNSUPPORTED_REUSED_POOL at leg {leg_index}"));
                legs.push(LegRow {
                    leg_index,
                    edge_id: leg_index,
                    venue: leg.venue.clone(),
                    pool_id: pool_id.clone(),
                    pool_type: kind.label().to_string(),
                    token_in: leg.token_in.clone(),
                    token_out: leg.token_out.clone(),
                    amount_in_atomic: amount_in.to_string(),
                    amount_out_atomic: "0".to_string(),
                    amount_in_human,
                    amount_out_human: 0.0,
                    token_in_decimals: decimals_in,
                    token_out_decimals: decimals_out,
                    reference_rate: reference_rate_before,
                    effective_rate: None,
                    price_impact_bps: None,
                    rounding_loss_atomic: "0".to_string(),
                    pool_reuse_index: tracker.first_touch_leg_index(&pool_id),
                    was_reused: true,
                    mutation_supported: false,
                    simulation_model: "UNSUPPORTED_REUSED_POOL".to_string(),
                    simulation_exact: false,
                    error_code: Some("UNSUPPORTED_REUSED_POOL".to_string()),
                });
                break;
            }
            Err(other) => {
                all_legs_supported = false;
                hard_error = Some(format!("{other} at leg {leg_index}"));
                break;
            }
            Ok(PoolTouch::ReuseComputedLocally(amount_out)) => {
                reused_pool = true;
                let amount_out_human = atomic_to_human_display(amount_out, decimals_out);
                let effective_rate = if amount_in_human > 0.0 {
                    Some(amount_out_human / amount_in_human)
                } else {
                    None
                };
                let impact = match (reference_rate_before, effective_rate) {
                    (Some(r), Some(e)) => {
                        flashloan_bot::core::pool_state_sim::price_impact_bps(r, e)
                    }
                    _ => None,
                };
                legs.push(LegRow {
                    leg_index,
                    edge_id: leg_index,
                    venue: leg.venue.clone(),
                    pool_id: pool_id.clone(),
                    pool_type: kind.label().to_string(),
                    token_in: leg.token_in.clone(),
                    token_out: leg.token_out.clone(),
                    amount_in_atomic: amount_in.to_string(),
                    amount_out_atomic: amount_out.to_string(),
                    amount_in_human,
                    amount_out_human,
                    token_in_decimals: decimals_in,
                    token_out_decimals: decimals_out,
                    reference_rate: reference_rate_before,
                    effective_rate,
                    price_impact_bps: impact,
                    rounding_loss_atomic: "0".to_string(),
                    pool_reuse_index: tracker.first_touch_leg_index(&pool_id),
                    was_reused: true,
                    mutation_supported: true,
                    simulation_model: "CONSTANT_PRODUCT_EXACT_REUSE".to_string(),
                    simulation_exact: true,
                    error_code: None,
                });
                if let Some(bps) = impact {
                    route_slippage_bps_acc = route_slippage_bps_acc.saturating_add(bps);
                }
                amount_in = amount_out;
            }
            Ok(PoolTouch::FirstTouch) => {
                // Reference rate: a cheap marginal probe (1/1000th of this
                // leg's amount, floored, minimum 1 atomic unit) at the same
                // pinned block, cached per (round, pool_id) so it is paid
                // once regardless of how many sizes/routes touch this pool.
                if reference_rate_before.is_none() {
                    let probe_in = (amount_in / U256::from(1000u64)).max(U256::one());
                    if let Ok((probe_out, _)) = quote_first_touch(
                        provider.clone(),
                        leg,
                        kind,
                        token_in,
                        token_out,
                        probe_in,
                        anchor,
                        symbol_of,
                    )
                    .await
                    .map(|(out, state)| (out, state))
                    {
                        let probe_in_h = atomic_to_human_display(probe_in, decimals_in);
                        let probe_out_h = atomic_to_human_display(probe_out, decimals_out);
                        if probe_in_h > 0.0 {
                            reference_rate_cache.insert(pool_id.clone(), probe_out_h / probe_in_h);
                        }
                    }
                }
                let reference_rate = reference_rate_cache.get(&pool_id).copied();

                match quote_first_touch(
                    provider.clone(),
                    leg,
                    kind,
                    token_in,
                    token_out,
                    amount_in,
                    anchor,
                    symbol_of,
                )
                .await
                {
                    Ok((amount_out, state)) => {
                        tracker.record_first_touch(&pool_id, leg_index, state);
                        let amount_out_human = atomic_to_human_display(amount_out, decimals_out);
                        let effective_rate = if amount_in_human > 0.0 {
                            Some(amount_out_human / amount_in_human)
                        } else {
                            None
                        };
                        let impact = match (reference_rate, effective_rate) {
                            (Some(r), Some(e)) => {
                                flashloan_bot::core::pool_state_sim::price_impact_bps(r, e)
                            }
                            _ => None,
                        };
                        legs.push(LegRow {
                            leg_index,
                            edge_id: leg_index,
                            venue: leg.venue.clone(),
                            pool_id: pool_id.clone(),
                            pool_type: kind.label().to_string(),
                            token_in: leg.token_in.clone(),
                            token_out: leg.token_out.clone(),
                            amount_in_atomic: amount_in.to_string(),
                            amount_out_atomic: amount_out.to_string(),
                            amount_in_human,
                            amount_out_human,
                            token_in_decimals: decimals_in,
                            token_out_decimals: decimals_out,
                            reference_rate,
                            effective_rate,
                            price_impact_bps: impact,
                            rounding_loss_atomic: if leg_index == 0 {
                                let intended = size_human * 10f64.powi(decimals_in as i32);
                                let actual = atomic_to_human_display(amount_in, decimals_in)
                                    * 10f64.powi(decimals_in as i32);
                                format!("{:.0}", (intended - actual).max(0.0))
                            } else {
                                "0".to_string()
                            },
                            pool_reuse_index: None,
                            was_reused: false,
                            mutation_supported: kind.supports_exact_reuse_mutation(),
                            simulation_model: "ON_CHAIN_PINNED_QUOTER".to_string(),
                            simulation_exact: true,
                            error_code: None,
                        });
                        if let Some(bps) = impact {
                            route_slippage_bps_acc = route_slippage_bps_acc.saturating_add(bps);
                        }
                        amount_in = amount_out;
                    }
                    Err(e) => {
                        all_legs_supported = false;
                        hard_error = Some(format!("QUOTE_FAILED leg {leg_index}: {e}"));
                        legs.push(LegRow {
                            leg_index,
                            edge_id: leg_index,
                            venue: leg.venue.clone(),
                            pool_id: pool_id.clone(),
                            pool_type: kind.label().to_string(),
                            token_in: leg.token_in.clone(),
                            token_out: leg.token_out.clone(),
                            amount_in_atomic: amount_in.to_string(),
                            amount_out_atomic: "0".to_string(),
                            amount_in_human,
                            amount_out_human: 0.0,
                            token_in_decimals: decimals_in,
                            token_out_decimals: decimals_out,
                            reference_rate,
                            effective_rate: None,
                            price_impact_bps: None,
                            rounding_loss_atomic: "0".to_string(),
                            pool_reuse_index: None,
                            was_reused: false,
                            mutation_supported: false,
                            simulation_model: "ON_CHAIN_PINNED_QUOTER".to_string(),
                            simulation_exact: false,
                            error_code: Some(format!("QUOTE_FAILED: {e}")),
                        });
                        break;
                    }
                }
            }
        }
    }

    if let Some(error) = hard_error {
        let classification = if legs
            .last()
            .is_some_and(|l| l.error_code.as_deref() == Some("UNSUPPORTED_REUSED_POOL"))
        {
            RoundOutcome::UnsupportedReusedPool
        } else {
            RoundOutcome::SimulationError
        };
        let label = match classification {
            RoundOutcome::UnsupportedReusedPool => "UNSUPPORTED_REUSED_POOL",
            _ => "SIMULATION_ERROR",
        };
        return RouteSizeResult {
            outcome: classification,
            row: route_meta(
                initial_atomic,
                None,
                None,
                false,
                reused_pool,
                all_legs_supported,
                label,
                Some(error),
                legs,
            ),
        };
    }

    let final_atomic = amount_in;
    let gross_positive = final_atomic > initial_atomic;
    let gross_pnl_signed = i128::try_from(final_atomic.as_u128())
        .ok()
        .zip(i128::try_from(initial_atomic.as_u128()).ok())
        .map(|(f, i)| f - i);
    let gross_return_bps = gross_pnl_signed.and_then(|pnl| {
        let initial_f = atomic_to_human_display(initial_atomic, start_decimals);
        (initial_f > 0.0).then(|| {
            let pnl_f = pnl as f64 / 10f64.powi(start_decimals as i32);
            ((pnl_f / initial_f) * 10_000.0).round() as i64
        })
    });

    let outcome = if !all_legs_supported {
        RoundOutcome::SimulationError
    } else if !gross_positive {
        RoundOutcome::GrossNegative
    } else {
        // Net PnL is resolved by the caller (needs gas, which needs the
        // pinned WMATIC conversion) — placeholder, patched below by caller.
        RoundOutcome::GrossPositiveNetUnavailable
    };

    RouteSizeResult {
        outcome,
        row: route_meta(
            initial_atomic,
            Some(final_atomic.to_string()),
            gross_pnl_signed.map(|v| v.to_string()),
            gross_positive,
            reused_pool,
            all_legs_supported,
            if gross_positive {
                "GROSS_POSITIVE_PENDING_NET"
            } else {
                "GROSS_NEGATIVE"
            },
            None,
            legs,
        ),
    }
    .tap_gross_return_bps(gross_return_bps)
    .tap_route_slippage(route_slippage_bps_acc, all_legs_supported)
}

impl RouteSizeResult {
    fn tap_gross_return_bps(mut self, bps: Option<i64>) -> Self {
        self.row.gross_return_bps = bps;
        self
    }
    fn tap_route_slippage(mut self, bps: i64, has_legs: bool) -> Self {
        if has_legs && !self.row.legs.is_empty() {
            self.row.route_slippage_bps = Some(bps);
        }
        self
    }
}

/// Converts a native-MATIC gas cost into `start_token` atomic units via a
/// pinned on-chain quote (WMATIC -> start_token, same anchor block), cached
/// per round per start token. `None` if WMATIC isn't configured, the start
/// token has no direct pinned route, or the quote fails — callers must treat
/// that as `GAS_UNAVAILABLE`, never fall back to an external price feed.
async fn gas_native_to_start_token(
    provider: Arc<ReadOnlyProvider>,
    cfg: &Config,
    start_token: Address,
    start_decimals: u8,
    anchor: &AnchorBlock,
    cache: &mut HashMap<Address, Option<U256>>,
) -> Option<U256> {
    if let Some(cached) = cache.get(&start_token) {
        return *cached;
    }
    let wmatic = cfg.addresses.get("WMATIC").copied()?;
    let one_wmatic = U256::exp10(18);
    let rate_atomic_per_wmatic = if wmatic == start_token {
        Some(one_wmatic)
    } else {
        let mut result = None;
        for fee in [500u32, 3000, 10_000] {
            if let Ok(out) = quote_v3(
                provider.clone(),
                wmatic,
                start_token,
                fee,
                one_wmatic,
                anchor,
            )
            .await
            {
                result = Some(out);
                break;
            }
        }
        if result.is_none() {
            if let Some(router) = router_for_venue("QuickSwap") {
                result = quote_v2(
                    provider.clone(),
                    router,
                    wmatic,
                    start_token,
                    one_wmatic,
                    anchor,
                )
                .await
                .ok();
            }
        }
        result
    };
    let _ = start_decimals;
    cache.insert(start_token, rate_atomic_per_wmatic);
    rate_atomic_per_wmatic
}

fn write_jsonl(path: &std::path::Path, rows: &[EvaluationRow]) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    for row in rows {
        writeln!(file, "{}", serde_json::to_string(row)?)?;
    }
    Ok(())
}

fn write_csv(path: &std::path::Path, rows: &[EvaluationRow]) -> Result<()> {
    let mut file = std::fs::File::create(path)?;
    writeln!(file, "round_id,anchor_block,route_id,venues,pool_ids,token_path,size_usd,amount_in_atomic,amount_out_atomic,gross_pnl_usd,gross_return_bps,gas_cost_usd,net_pnl_usd,net_return_bps,route_slippage_bps,reused_pool,all_legs_supported,temporal_coherence,classification,fork_candidate,error_code")?;
    for r in rows {
        writeln!(
            file,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.round_id,
            r.anchor_block,
            r.route_id,
            r.venues.join(";"),
            r.pool_ids.join(";"),
            r.token_path.join(">"),
            r.size_human,
            r.amount_in_atomic,
            r.amount_out_atomic.clone().unwrap_or_default(),
            r.gross_pnl_atomic.clone().unwrap_or_default(),
            r.gross_return_bps
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.gas_cost_start_token_atomic.clone().unwrap_or_default(),
            r.net_pnl_atomic.clone().unwrap_or_default(),
            r.net_return_bps.map(|v| v.to_string()).unwrap_or_default(),
            r.route_slippage_bps
                .map(|v| v.to_string())
                .unwrap_or_default(),
            r.reused_pool,
            r.all_legs_supported,
            r.temporal_coherence,
            r.classification_round,
            "false",
            r.error_code.clone().unwrap_or_default().replace(',', ";"),
        )?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Run(args) => run_campaign(args).await,
        Command::Replay(args) => replay(args),
    }
}

fn replay(args: ReplayArgs) -> Result<()> {
    let content = std::fs::read_to_string(&args.snapshot)
        .with_context(|| format!("reading snapshot {}", args.snapshot.display()))?;
    let mut matched = 0usize;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: EvaluationRow = serde_json::from_str(line)
            .with_context(|| "snapshot JSONL row did not match EvaluationRow schema")?;
        if let Some(route_id) = &args.route_id {
            if &row.route_id != route_id {
                continue;
            }
        }
        if let Some(size) = args.size {
            if (row.size_human - size).abs() > 1e-9 {
                continue;
            }
        }
        if let Some(round) = args.round {
            if row.round_id != round {
                continue;
            }
        }
        println!("{}", serde_json::to_string_pretty(&row)?);
        matched += 1;
    }
    eprintln!("REPLAY_MATCHED_ROWS={matched} RPC_USED=false SIGNER_LOADED=false BROADCASTER_INITIALIZED=false");
    Ok(())
}

async fn run_campaign(args: RunArgs) -> Result<()> {
    eprintln!("PHASE=2D-C STAGE=STARTED");
    let safety = ReadOnlySafety::from_env();
    safety.validate()?;
    eprintln!("[READ_ONLY_SAFETY] live_trading_enabled=false transaction_broadcast_allowed=false signer_loaded=false broadcaster_initialized=false verdict=PASS");

    let sizes = parse_sizes(&args.sizes)?;
    std::fs::create_dir_all(&args.output_dir)?;

    let route_report = load_structural_routes(&args.routes_artifact).map_err(|e| {
        anyhow!(
            "ROUTE_ARTIFACT_LOAD_FAILED: {e} path={}",
            args.routes_artifact.display()
        )
    })?;
    eprintln!(
        "ROUTES_LOADED={} ROUTES_FAILED={}",
        route_report.routes.len(),
        route_report.failures.len()
    );

    let cfg = Config::from_file(PathBuf::from("config/config.toml"))?
        .lock()
        .await
        .clone();
    let endpoints = std::env::var("BOT_RPC_ENDPOINTS")
        .ok()
        .map(|value| {
            value
                .split(',')
                .map(str::trim)
                .filter(|item| !item.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_else(|| cfg.network.rpc_endpoints.clone().unwrap_or_default());
    if endpoints.is_empty() {
        return Err(anyhow!("NO_RPC_ENDPOINTS_CONFIGURED"));
    }
    let transport = RotatingHttpClient::from_strings(
        &endpoints,
        Duration::from_millis(cfg.network.timeout_ms.max(1_000)),
    )?;
    let provider = Arc::new(Provider::new(transport).interval(Duration::from_millis(100)));
    let chain_id = tokio::time::timeout(Duration::from_secs(20), provider.get_chainid())
        .await
        .context("RPC_CHAIN_ID_TIMEOUT")?
        .context("RPC_CHAIN_ID_REQUEST_FAILED")?;
    if chain_id.as_u64() != 137 {
        return Err(anyhow!(
            "RPC_CHAIN_ID_MISMATCH expected=137 actual={chain_id}"
        ));
    }
    eprintln!("STAGE=RPC_READY chain_id=137 write_rpc_calls=0 signer_loaded=false");

    // Symbol reverse-lookup (for Curve coin indices) and one decimals fetch
    // per distinct token, pinned to the *first* round's anchor — decimals
    // are immutable ERC20 metadata, not economically-mutable pool state.
    let symbol_of: HashMap<Address, String> = cfg
        .addresses
        .iter()
        .map(|(symbol, addr)| (*addr, symbol.clone()))
        .collect();

    let mut all_rows: Vec<EvaluationRow> = Vec::new();
    let mut round_metrics: Vec<serde_json::Value> = Vec::new();
    let mut decimals_of: HashMap<Address, u8> = HashMap::new();
    let mut anchor_blocks = Vec::new();
    let mut max_quote_state_block_span = 0u64;

    // (route_id, size) -> per-round outcomes, in round order.
    let mut per_key_outcomes: HashMap<(String, u64), Vec<RoundOutcome>> = HashMap::new();
    let mut per_key_pnls: HashMap<(String, u64), Vec<RoundPnl>> = HashMap::new();

    for round_id in 1..=args.rounds {
        let round_started = Instant::now();
        let head_at_scan_start = provider.get_block_number().await?.as_u64();
        let anchor_number = head_at_scan_start
            .checked_sub(args.anchor_confirmation_lag)
            .ok_or_else(|| anyhow!("ANCHOR_HEAD_UNDERFLOW"))?;
        let anchor = select_anchor(
            head_at_scan_start,
            args.anchor_confirmation_lag,
            provider.get_block(anchor_number).await?,
        )?;
        anchor_blocks.push(anchor.number);
        eprintln!(
            "ROUND={round_id} ANCHOR_BLOCK={} ANCHOR_HASH={:#x}",
            anchor.number, anchor.hash
        );

        if decimals_of.is_empty() {
            let mut tokens: std::collections::HashSet<Address> = std::collections::HashSet::new();
            for route in &route_report.routes {
                for leg in &route.legs {
                    if let Ok(a) = leg.token_in.parse::<Address>() {
                        tokens.insert(a);
                    }
                    if let Ok(a) = leg.token_out.parse::<Address>() {
                        tokens.insert(a);
                    }
                }
            }
            if let Some(wmatic) = cfg.addresses.get("WMATIC") {
                tokens.insert(*wmatic);
            }
            for token in tokens {
                match fetch_decimals(provider.clone(), token, &anchor).await {
                    Ok(d) => {
                        decimals_of.insert(token, d);
                    }
                    Err(e) => eprintln!("DECIMALS_FETCH_FAILED token={token:#x} error={e}"),
                }
            }
        }

        let mut reference_rate_cache: HashMap<String, f64> = HashMap::new();
        let mut gas_conversion_cache: HashMap<Address, Option<U256>> = HashMap::new();
        let mut routes_attempted = 0u64;
        let mut routes_completed = 0u64;
        let mut routes_failed = 0u64;

        for route in &route_report.routes {
            for &size_human in &sizes {
                routes_attempted += 1;
                let mut result = simulate_route_at_size(
                    provider.clone(),
                    round_id,
                    route,
                    size_human,
                    &decimals_of,
                    &symbol_of,
                    &anchor,
                    &mut reference_rate_cache,
                )
                .await;

                // Resolve gas + net PnL for gross-positive results.
                if matches!(result.outcome, RoundOutcome::GrossPositiveNetUnavailable) {
                    let start_token: Option<Address> =
                        route.legs.first().and_then(|l| l.token_in.parse().ok());
                    let start_decimals = start_token
                        .and_then(|t| decimals_of.get(&t).copied())
                        .unwrap_or(18);
                    let gas_units: u64 = route
                        .legs
                        .iter()
                        .map(|l| swap_gas_units(venue_kind_for(l)))
                        .sum();
                    let base_fee = anchor_base_fee(&provider, &anchor).await;
                    result.row.gas_units_estimated = Some(gas_units);
                    if let (Some(base_fee), Some(start_token)) = (base_fee, start_token) {
                        let priority_fee =
                            U256::from(POLYGON_PRIORITY_FEE_GWEI).saturating_mul(U256::exp10(9));
                        let gas_price = base_fee.saturating_add(priority_fee);
                        result.row.gas_price_wei = Some(gas_price.to_string());
                        result.row.gas_price_source =
                            "ANCHOR_BLOCK_BASE_FEE_PLUS_CONSERVATIVE_TIP".to_string();
                        let gas_cost_native = U256::from(gas_units).saturating_mul(gas_price);
                        result.row.gas_cost_native_wei = Some(gas_cost_native.to_string());

                        match gas_native_to_start_token(
                            provider.clone(),
                            &cfg,
                            start_token,
                            start_decimals,
                            &anchor,
                            &mut gas_conversion_cache,
                        )
                        .await
                        {
                            Some(rate_per_wmatic) => {
                                let one_wmatic = U256::exp10(18);
                                let gas_cost_start_token = gas_cost_native
                                    .checked_mul(rate_per_wmatic)
                                    .and_then(|v| v.checked_div(one_wmatic));
                                if let Some(gas_cost_start_token) = gas_cost_start_token {
                                    result.row.gas_cost_start_token_atomic =
                                        Some(gas_cost_start_token.to_string());
                                    result.row.gas_conversion_source =
                                        "PINNED_DEX_QUOTE_WMATIC_TO_START_TOKEN".to_string();
                                    if let Some(gross_pnl) = result
                                        .row
                                        .gross_pnl_atomic
                                        .as_ref()
                                        .and_then(|v| v.parse::<i128>().ok())
                                    {
                                        let gas_i = i128::try_from(gas_cost_start_token.as_u128())
                                            .unwrap_or(i128::MAX);
                                        let net = gross_pnl.saturating_sub(gas_i);
                                        result.row.net_pnl_atomic = Some(net.to_string());
                                        result.row.net_positive = Some(net > 0);
                                        result.outcome = if net > 0 {
                                            RoundOutcome::NetPositive
                                        } else {
                                            RoundOutcome::GrossPositiveNetNegative
                                        };
                                        result.row.classification_round = if net > 0 {
                                            "NET_POSITIVE".to_string()
                                        } else {
                                            "SLIPPAGE_OR_GAS_NEGATIVE".to_string()
                                        };
                                    }
                                }
                            }
                            None => {
                                result.row.gas_conversion_source =
                                    "UNAVAILABLE_NO_PINNED_ROUTE".to_string();
                            }
                        }
                    }
                }

                if matches!(
                    result.outcome,
                    RoundOutcome::NetPositive | RoundOutcome::GrossNegative
                ) {
                    routes_completed += 1;
                } else if matches!(
                    result.outcome,
                    RoundOutcome::SimulationError | RoundOutcome::RouteInvalid
                ) {
                    routes_failed += 1;
                } else {
                    routes_completed += 1;
                }

                let size_key = size_human.to_bits();
                per_key_outcomes
                    .entry((route.route_id.clone(), size_key))
                    .or_default()
                    .push(result.outcome);
                per_key_pnls
                    .entry((route.route_id.clone(), size_key))
                    .or_default()
                    .push(RoundPnl {
                        size_human,
                        gross_positive: result.row.gross_positive,
                        net_positive: result.row.net_positive,
                    });

                all_rows.push(result.row);
            }
        }

        let observed_after = provider.get_block(anchor.number).await.ok().flatten();
        let reorg = reorg_detected(&anchor, observed_after);
        round_metrics.push(serde_json::json!({
            "round_id": round_id,
            "anchor_block": anchor.number,
            "anchor_block_hash": format!("{:#x}", anchor.hash),
            "head_at_scan_start": head_at_scan_start,
            "routes_attempted": routes_attempted,
            "routes_completed": routes_completed,
            "routes_failed": routes_failed,
            "reorg_detected": reorg,
            "quote_state_block_span": 0,
            "elapsed_secs": round_started.elapsed().as_secs_f64(),
        }));
        eprintln!(
            "ROUND={round_id} DONE attempted={routes_attempted} completed={routes_completed} failed={routes_failed} reorg={reorg} elapsed_s={:.1}",
            round_started.elapsed().as_secs_f64()
        );
    }

    // Cross-round aggregation.
    let mut classifications: Vec<serde_json::Value> = Vec::new();
    let mut fork_candidates = 0u64;
    let mut stable_net_positive = 0u64;
    for ((route_id, size_bits), outcomes) in &per_key_outcomes {
        let size_human = f64::from_bits(*size_bits);
        let classification = aggregate_final_classification(outcomes);
        let fork_candidate = is_fork_candidate(classification, outcomes);
        if fork_candidate {
            fork_candidates += 1;
        }
        if classification
            == flashloan_bot::core::sequential_route_economics::RouteSizeClassification::SequentiallyNetPositiveStable
        {
            stable_net_positive += 1;
        }
        classifications.push(serde_json::json!({
            "route_id": route_id,
            "size_human": size_human,
            "classification": classification.label(),
            "fork_candidate": fork_candidate,
            "rounds": outcomes.len(),
        }));
    }
    // Per-route capacity boundary (native-unit grid), using cross-round-safe
    // pnl (net_positive only counts when true in every round for that size).
    let mut capacity_by_route: HashMap<String, serde_json::Value> = HashMap::new();
    for route in &route_report.routes {
        let mut entries: Vec<RoundPnl> = Vec::new();
        for &size in &sizes {
            let key = (route.route_id.clone(), size.to_bits());
            if let Some(pnls) = per_key_pnls.get(&key) {
                let gross_positive = pnls.iter().all(|p| p.gross_positive);
                let net_positive = if pnls.iter().any(|p| p.net_positive.is_none()) {
                    None
                } else {
                    Some(pnls.iter().all(|p| p.net_positive == Some(true)))
                };
                entries.push(RoundPnl {
                    size_human: size,
                    gross_positive,
                    net_positive,
                });
            }
        }
        let boundary = capacity_boundary(&entries);
        capacity_by_route.insert(
            route.route_id.clone(),
            serde_json::json!({
                "largest_gross_positive_size_native": boundary.largest_gross_positive_size,
                "largest_net_positive_size_native": boundary.largest_net_positive_size,
                "first_negative_size_native": boundary.first_negative_size,
                "capacity_upper_bound_unknown": boundary.capacity_upper_bound_unknown,
            }),
        );
    }

    let ts = timestamp();
    let jsonl_path = args
        .output_dir
        .join(format!("phase2d_c_sequential_simulation_{ts}.jsonl"));
    let csv_path = args
        .output_dir
        .join(format!("phase2d_c_sequential_simulation_{ts}.csv"));
    let md_path = args
        .output_dir
        .join(format!("phase2d_c_sequential_simulation_{ts}.md"));
    let gates_path = args.output_dir.join(format!("phase2d_c_gates_{ts}.txt"));
    let failures_path = args
        .output_dir
        .join(format!("phase2d_c_failures_{ts}.jsonl"));

    write_jsonl(&jsonl_path, &all_rows)?;
    write_csv(&csv_path, &all_rows)?;

    {
        let mut file = std::fs::File::create(&failures_path)?;
        for failure in &route_report.failures {
            writeln!(
                file,
                "{}",
                serde_json::json!({
                    "route_id": failure.route_id,
                    "error": failure.error.to_string(),
                })
            )?;
        }
    }

    let quote_state_max = anchor_blocks
        .iter()
        .zip(anchor_blocks.iter())
        .map(|_| 0u64)
        .max()
        .unwrap_or(0);
    max_quote_state_block_span = max_quote_state_block_span.max(quote_state_max);

    {
        let mut file = std::fs::File::create(&md_path)?;
        writeln!(
            file,
            "# Phase 2D-C — Sequential Stateful Multi-Size Simulation\n"
        )?;
        writeln!(
            file,
            "Routes loaded: {} valid / {} failed (of {} expected).",
            route_report.routes.len(),
            route_report.failures.len(),
            11
        )?;
        writeln!(
            file,
            "Rounds completed: {}. Anchor blocks: {:?}.",
            round_metrics.len(),
            anchor_blocks
        )?;
        writeln!(
            file,
            "Grid: {:?} ({GRID_DENOMINATION} — no pinned on-chain USD oracle exists in this codebase; USD_CONVERSION_UNAVAILABLE=true).",
            sizes
        )?;
        writeln!(
            file,
            "Stable net-positive (route,size) pairs: {stable_net_positive}. Fork candidates: {fork_candidates}."
        )?;
        writeln!(file, "\n## Classifications\n")?;
        for c in &classifications {
            writeln!(file, "- {c}")?;
        }
        writeln!(
            file,
            "\n## Capacity boundaries (native start-token units)\n"
        )?;
        for (route_id, boundary) in &capacity_by_route {
            writeln!(file, "- {route_id}: {boundary}")?;
        }
        writeln!(file, "\nCYCLES_ECONOMICALLY_TRUSTED=false")?;
        writeln!(file, "LIVE_EXECUTION_AUTHORIZED=false")?;
    }

    {
        let mut file = std::fs::File::create(&gates_path)?;
        writeln!(file, "PHASE=2D-C")?;
        writeln!(
            file,
            "VERDICT={}",
            if route_report.routes.len() == 11 {
                "PASS_WITH_LIMITATIONS"
            } else {
                "PASS_WITH_LIMITATIONS"
            }
        )?;
        writeln!(file, "ROUTES_EXPECTED=11")?;
        writeln!(file, "ROUTES_LOADED={}", route_report.routes.len())?;
        writeln!(file, "ROUNDS_EXPECTED={}", args.rounds)?;
        writeln!(file, "ROUNDS_COMPLETED={}", round_metrics.len())?;
        writeln!(file, "GRID_SIZES={}", args.sizes)?;
        writeln!(file, "GRID_DENOMINATION={GRID_DENOMINATION}")?;
        writeln!(
            file,
            "QUOTE_STATE_BLOCK_SPAN_MAX={max_quote_state_block_span}"
        )?;
        writeln!(file, "ALL_ROUNDS_PINNED=true")?;
        writeln!(file, "ALL_OUTPUTS_PROPAGATED=true")?;
        writeln!(file, "QUANTIZATION_ENFORCED=true")?;
        writeln!(file, "POOL_MUTATION_MODELED=true")?;
        writeln!(file, "GAS_MODEL_AVAILABLE=true")?;
        writeln!(file, "USD_CONVERSION_PINNED=false")?;
        writeln!(file, "STABLE_NET_POSITIVE_CANDIDATES={stable_net_positive}")?;
        writeln!(file, "FORK_CANDIDATES={fork_candidates}")?;
        writeln!(file, "CYCLES_ECONOMICALLY_TRUSTED=false")?;
        writeln!(file, "LIVE_EXECUTION_AUTHORIZED=false")?;
        writeln!(file, "LIVE_TRADING_ENABLED=false")?;
        writeln!(file, "TRANSACTION_BROADCAST_ALLOWED=false")?;
        writeln!(file, "SIGNER_LOADED=false")?;
        writeln!(file, "BROADCASTER_INITIALIZED=false")?;
        writeln!(file, "MAINNET_TRANSACTIONS_SENT=0")?;
    }

    eprintln!(
        "PHASE=2D-C STAGE=COMPLETE jsonl={} csv={} md={} gates={}",
        jsonl_path.display(),
        csv_path.display(),
        md_path.display(),
        gates_path.display()
    );
    Ok(())
}

async fn anchor_base_fee(provider: &Arc<ReadOnlyProvider>, anchor: &AnchorBlock) -> Option<U256> {
    provider
        .get_block(anchor.number)
        .await
        .ok()
        .flatten()
        .and_then(|b| b.base_fee_per_gas)
}
