//! Replay scan: varrer blocos históricos (horário ativo) e medir edges triangulares.
//!
//! Quote e eth_call usam o **mesmo** `blockTag`. Paper-only (`observation_active`);
//! envio permanece bloqueado. Não altera o finder — só alimenta price_map@block.

use crate::{
    config::{token_cache::TokenCache, Config, ReplayConfig},
    contracts::ERC20,
    core::{
        arbitrage::ArbitrageEngine,
        flashloan::ArbitrageClient,
        paper_validation,
        types::{ArbitrageOpportunity, ArbitrageStep},
    },
    dex::{
        cache_fee_tier, calculate_price_from_decimals, quote_amount_for_usd,
        EXECUTABLE_V3_FEE_TIERS,
    },
    AppMiddleware,
};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, TimeZone, Utc, Weekday};
use ethers::{
    abi::Abi,
    contract::Contract,
    prelude::*,
    types::{Address, BlockId, BlockNumber, U256},
};
use serde::Serialize;
use std::{
    collections::{BTreeSet, HashMap},
    fs::OpenOptions,
    io::Write,
    path::Path,
    str::FromStr,
    sync::Arc,
};
use tracing::{info, warn};

pub const ENV_REPLAY_SCAN: &str = "REPLAY_SCAN";

/// ~2.0s/bloco Polygon (estimativa timestamp→block).
const POLYGON_BLOCK_SECS: f64 = 2.0;

const V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUICKSWAP_ROUTER: &str = "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff";
const SUSHISWAP_ROUTER: &str = "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506";

// ---------------------------------------------------------------------------
// Gate
// ---------------------------------------------------------------------------

/// Scan só com paper observation ativo; envio continua proibido.
pub fn replay_scan_allowed(cfg: &Config) -> bool {
    let env_on = std::env::var(ENV_REPLAY_SCAN)
        .map(|v| matches!(v.to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
        .unwrap_or(false);
    let cfg_on = cfg.validation.replay.enabled;
    (env_on || cfg_on) && paper_validation::observation_active(cfg)
}

pub fn should_run(cfg: &Config) -> bool {
    replay_scan_allowed(cfg)
}

/// Piso teórico: ∏(1 − fee_i) com fees dos steps (V3=tier/1e6, V2=30bps).
pub fn theoretical_fee_floor(steps: &[ArbitrageStep]) -> f64 {
    let mut p = 1.0_f64;
    for s in steps {
        let fee = if s.dex_name.to_ascii_lowercase().contains("uniswapv3") {
            s.v3_fee_tier.unwrap_or(3000) as f64 / 1_000_000.0
        } else {
            0.003
        };
        p *= 1.0 - fee;
    }
    p
}

pub fn edge_exists(cycle_rate: f64) -> bool {
    cycle_rate.is_finite() && cycle_rate > 1.0
}

/// Invariante: quote e eth_call no mesmo blockTag.
pub fn assert_same_block_tag(quote_block: u64, eth_call_block: u64) -> Result<()> {
    if quote_block != eth_call_block {
        bail!(
            "replay blockTag mismatch: quote={} eth_call={}",
            quote_block,
            eth_call_block
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Sample + aggregate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ReplayBlockSample {
    pub block: u64,
    pub quote_block: u64,
    pub eth_call_block: Option<u64>,
    pub best_cycle_rate: f64,
    pub fee_floor: f64,
    pub edge_exists: bool,
    pub pair: String,
    pub route: String,
    pub fee_tiers: String,
    pub net_previsto_usd: Option<f64>,
    pub profit_realizado_usd: Option<f64>,
    pub erro_rel_pct: Option<f64>,
    pub sim_ok: Option<bool>,
    pub revert_reason: Option<String>,
    pub archive_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReplayScanSummary {
    pub n_blocos_amostrados: u64,
    pub block_from: u64,
    pub block_to: u64,
    pub step: u64,
    pub best_cycle_rate_min: Option<f64>,
    pub best_cycle_rate_p50: Option<f64>,
    pub best_cycle_rate_p95: Option<f64>,
    pub best_cycle_rate_max: Option<f64>,
    pub n_blocos_com_edge: u64,
    pub n_blocos_lucrativos_pos_custos: u64,
    pub n_reached_eth_call: u64,
    pub n_sim_ok: u64,
    pub n_reverts: u64,
    pub revert_reasons: Vec<(String, u64)>,
    pub n_archive_abort: u64,
    pub erro_rel_pct_p50: Option<f64>,
    pub erro_rel_pct_p95: Option<f64>,
    pub top5: Vec<(u64, f64, String)>,
}

impl ReplayScanSummary {
    pub fn from_samples(samples: &[ReplayBlockSample], from: u64, to: u64, step: u64) -> Self {
        let mut rates: Vec<f64> = samples
            .iter()
            .map(|s| s.best_cycle_rate)
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let n_edge = samples.iter().filter(|s| s.edge_exists).count() as u64;
        let n_prof = samples
            .iter()
            .filter(|s| {
                s.sim_ok == Some(true)
                    && s.profit_realizado_usd.map(|p| p > 0.0).unwrap_or(false)
            })
            .count() as u64;
        let n_eth = samples.iter().filter(|s| s.eth_call_block.is_some()).count() as u64;
        let n_ok = samples.iter().filter(|s| s.sim_ok == Some(true)).count() as u64;
        let n_arch = samples.iter().filter(|s| s.archive_error.is_some()).count() as u64;

        let mut reason_counts: HashMap<String, u64> = HashMap::new();
        let mut n_reverts = 0u64;
        for s in samples {
            if s.sim_ok == Some(false) || (s.eth_call_block.is_some() && s.sim_ok != Some(true)) {
                n_reverts += 1;
                if let Some(r) = &s.revert_reason {
                    let key = r.chars().take(120).collect::<String>();
                    *reason_counts.entry(key).or_default() += 1;
                }
            }
        }
        let mut revert_reasons: Vec<(String, u64)> = reason_counts.into_iter().collect();
        revert_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        revert_reasons.truncate(10);

        let mut errs: Vec<f64> = samples
            .iter()
            .filter(|s| s.sim_ok == Some(true))
            .filter_map(|s| s.erro_rel_pct)
            .collect();
        errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mut top: Vec<(u64, f64, String)> = samples
            .iter()
            .map(|s| (s.block, s.best_cycle_rate, s.pair.clone()))
            .collect();
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(5);

        Self {
            n_blocos_amostrados: samples.len() as u64,
            block_from: from,
            block_to: to,
            step,
            best_cycle_rate_min: rates.first().copied(),
            best_cycle_rate_p50: percentile(&rates, 50.0),
            best_cycle_rate_p95: percentile(&rates, 95.0),
            best_cycle_rate_max: rates.last().copied(),
            n_blocos_com_edge: n_edge,
            n_blocos_lucrativos_pos_custos: n_prof,
            n_reached_eth_call: n_eth,
            n_sim_ok: n_ok,
            n_reverts,
            revert_reasons,
            n_archive_abort: n_arch,
            erro_rel_pct_p50: percentile(&errs, 50.0),
            erro_rel_pct_p95: percentile(&errs, 95.0),
            top5: top,
        }
    }

    pub fn log(&self) {
        info!(
            target: "replay_scan",
            "📊 REPLAY SCAN SUMMARY | blocks={} range=[{}..{}] step={} | best_cycle_rate min={:?} p50={:?} p95={:?} max={:?} | n_edge={} n_lucrativos_pos_custos={} | eth_call={} sim_ok={} reverts={} archive_abort={} | erro_rel% p50={:?} p95={:?}",
            self.n_blocos_amostrados,
            self.block_from,
            self.block_to,
            self.step,
            self.best_cycle_rate_min,
            self.best_cycle_rate_p50,
            self.best_cycle_rate_p95,
            self.best_cycle_rate_max,
            self.n_blocos_com_edge,
            self.n_blocos_lucrativos_pos_custos,
            self.n_reached_eth_call,
            self.n_sim_ok,
            self.n_reverts,
            self.n_archive_abort,
            self.erro_rel_pct_p50,
            self.erro_rel_pct_p95,
        );
        for (reason, n) in &self.revert_reasons {
            info!(
                target: "replay_scan",
                "📊 REPLAY REVERT | n={} reason={}",
                n,
                reason
            );
        }
        for (i, (b, r, p)) in self.top5.iter().enumerate() {
            info!(
                target: "replay_scan",
                "📊 REPLAY TOP{} block={} rate={:.8} pair={}",
                i + 1,
                b,
                r,
                p
            );
        }
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).floor() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

// ---------------------------------------------------------------------------
// Block range
// ---------------------------------------------------------------------------

/// Última janela útil 14:30–17:30 UTC (abertura mercados US / overlap líquido).
pub fn resolve_us_session_window(latest_block: u64, latest_ts: u64) -> (u64, u64, String) {
    let now = Utc
        .timestamp_opt(latest_ts as i64, 0)
        .single()
        .unwrap_or_else(Utc::now);

    let mut day = now.date_naive();
    for _ in 0..10 {
        day -= ChronoDuration::days(1);
        if matches!(
            day.weekday(),
            Weekday::Mon | Weekday::Tue | Weekday::Wed | Weekday::Thu | Weekday::Fri
        ) {
            break;
        }
    }

    let start = day.and_hms_opt(14, 30, 0).unwrap();
    let end = day.and_hms_opt(17, 30, 0).unwrap();
    let start_ts = Utc.from_utc_datetime(&start).timestamp() as u64;
    let end_ts = Utc.from_utc_datetime(&end).timestamp() as u64;

    let from = estimate_block_at(latest_block, latest_ts, start_ts);
    let to = estimate_block_at(latest_block, latest_ts, end_ts);
    let label = format!(
        "US cash session {} 14:30–17:30 UTC (est. blocks {}–{})",
        day,
        from.min(to),
        from.max(to)
    );
    (from.min(to), from.max(to), label)
}

pub fn estimate_block_at(latest_block: u64, latest_ts: u64, target_ts: u64) -> u64 {
    let dt = latest_ts as i64 - target_ts as i64;
    let delta = (dt as f64 / POLYGON_BLOCK_SECS).round() as i64;
    (latest_block as i64 - delta).max(1) as u64
}

pub fn resolve_block_range(
    cfg: &ReplayConfig,
    latest_block: u64,
    latest_ts: u64,
) -> Result<(u64, u64, String)> {
    let from = cfg.block_from.filter(|&b| b > 0);
    let to = cfg.block_to.filter(|&b| b > 0);
    match (from, to) {
        (Some(a), Some(b)) => {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            Ok((lo, hi, format!("config range [{lo}..{hi}]")))
        }
        _ if cfg.auto_us_session => Ok(resolve_us_session_window(latest_block, latest_ts)),
        _ => bail!("replay: block_from/block_to ausentes e auto_us_session=false"),
    }
}

// ---------------------------------------------------------------------------
// Historical quotes
// ---------------------------------------------------------------------------

fn parse_abi_flexible(raw: &str) -> Result<Abi> {
    if let Ok(abi) = serde_json::from_str::<Abi>(raw) {
        return Ok(abi);
    }
    let v: serde_json::Value = serde_json::from_str(raw)?;
    let arr = v
        .get("abi")
        .cloned()
        .ok_or_else(|| anyhow!("ABI wrapper sem campo 'abi'"))?;
    serde_json::from_value(arr).context("parse ABI array")
}

struct HistQuoter {
    client: Arc<AppMiddleware>,
    token_cache: Arc<TokenCache>,
    quoter_abi: Abi,
    router_abi: Abi,
    notional_usd: f64,
}

impl HistQuoter {
    async fn new(client: Arc<AppMiddleware>, cfg: Arc<Config>) -> Result<Self> {
        let quoter_abi = parse_abi_flexible(include_str!("../../abi/uniswap_v3_quoter.json"))
            .context("parse uniswap_v3_quoter abi")?;
        let router_abi = parse_abi_flexible(include_str!("../../abi/uniswap_v2_router.json"))
            .context("parse uniswap_v2_router abi")?;
        let token_cache = TokenCache::global(cfg).await;
        Ok(Self {
            client,
            token_cache,
            quoter_abi,
            router_abi,
            notional_usd: 100.0,
        })
    }

    async fn resolve(&self, symbol: &str) -> Result<(Address, u8)> {
        let info = self
            .token_cache
            .get_by_symbol(symbol)
            .await
            .ok_or_else(|| anyhow!("token_cache miss: {symbol}"))?;
        Ok((info.address, info.decimals))
    }

    async fn quote_v3_best(
        &self,
        token_in: &str,
        token_out: &str,
        block: BlockId,
    ) -> Result<Option<(f64, u32)>> {
        let (addr_in, dec_in) = self.resolve(token_in).await?;
        let (addr_out, dec_out) = self.resolve(token_out).await?;
        let amount_in = quote_amount_for_usd(token_in, dec_in, self.notional_usd).await?;
        let quoter = Contract::new(
            Address::from_str(V3_QUOTER)?,
            self.quoter_abi.clone(),
            self.client.clone(),
        );

        let mut best: Option<(U256, u32)> = None;
        for &fee in &EXECUTABLE_V3_FEE_TIERS {
            let call = quoter.method::<_, U256>(
                "quoteExactInputSingle",
                (addr_in, addr_out, fee, amount_in, U256::zero()),
            )?;
            match call.block(block).call().await {
                Ok(out) if !out.is_zero() => {
                    if best.map(|(o, _)| out > o).unwrap_or(true) {
                        best = Some((out, fee));
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let s = e.to_string();
                    if paper_validation::is_archive_state_error(&s) {
                        return Err(anyhow!("archive state unavailable: {s}"));
                    }
                }
            }
        }
        let Some((out, fee)) = best else {
            return Ok(None);
        };
        let price = calculate_price_from_decimals(amount_in, out, dec_in, dec_out)?;
        if price <= 0.0 {
            return Ok(None);
        }
        cache_fee_tier("UniswapV3", token_in, token_out, fee);
        Ok(Some((price, fee)))
    }

    async fn quote_v2(
        &self,
        dex: &str,
        router: Address,
        token_in: &str,
        token_out: &str,
        block: BlockId,
    ) -> Result<Option<f64>> {
        let (addr_in, dec_in) = self.resolve(token_in).await?;
        let (addr_out, dec_out) = self.resolve(token_out).await?;
        let amount_in = quote_amount_for_usd(token_in, dec_in, self.notional_usd).await?;
        let router_c = Contract::new(router, self.router_abi.clone(), self.client.clone());
        let call = router_c.method::<_, Vec<U256>>(
            "getAmountsOut",
            (amount_in, vec![addr_in, addr_out]),
        )?;
        match call.block(block).call().await {
            Ok(amounts) if amounts.len() >= 2 && !amounts[1].is_zero() => {
                let price =
                    calculate_price_from_decimals(amount_in, amounts[1], dec_in, dec_out)?;
                Ok(if price > 0.0 { Some(price) } else { None })
            }
            Ok(_) => Ok(None),
            Err(e) => {
                let s = e.to_string();
                if paper_validation::is_archive_state_error(&s) {
                    return Err(anyhow!("archive state unavailable ({dex}): {s}"));
                }
                Ok(None)
            }
        }
    }

    async fn pool_tvl_usd(
        &self,
        pool: Address,
        token_a: Address,
        dec_a: u8,
        price_a: f64,
        token_b: Address,
        dec_b: u8,
        price_b: f64,
        block: BlockId,
    ) -> Result<f64> {
        let erc_a = ERC20::new(token_a, self.client.clone());
        let erc_b = ERC20::new(token_b, self.client.clone());
        let bal_a = erc_a.balance_of(pool).block(block).call().await.map_err(|e| {
            let s = e.to_string();
            if paper_validation::is_archive_state_error(&s) {
                anyhow!("archive state unavailable (balanceOf): {s}")
            } else {
                anyhow!("balanceOf A: {s}")
            }
        })?;
        let bal_b = erc_b.balance_of(pool).block(block).call().await.map_err(|e| {
            let s = e.to_string();
            if paper_validation::is_archive_state_error(&s) {
                anyhow!("archive state unavailable (balanceOf): {s}")
            } else {
                anyhow!("balanceOf B: {s}")
            }
        })?;
        Ok(crate::dex::liquidity::pool_tvl_usd_from_balances(
            bal_a, dec_a, price_a, bal_b, dec_b, price_b,
        ))
    }
}

fn triangular_pairs(cfg: &Config) -> Vec<(String, String)> {
    let mut set = BTreeSet::new();
    let mids = &cfg.arbitrage.triangular.midcaps;
    let anchors = &cfg.arbitrage.triangular.anchors;
    for m in mids {
        for a in anchors {
            if m.eq_ignore_ascii_case(a) {
                continue;
            }
            set.insert((m.clone(), a.clone()));
            set.insert((a.clone(), m.clone()));
        }
    }
    for i in 0..anchors.len() {
        for j in 0..anchors.len() {
            if i != j {
                set.insert((anchors[i].clone(), anchors[j].clone()));
            }
        }
    }
    set.into_iter().collect()
}

async fn build_price_map_at_block(
    quoter: &HistQuoter,
    cfg: &Config,
    block_num: u64,
) -> Result<HashMap<String, HashMap<String, f64>>> {
    let block = BlockId::Number(BlockNumber::Number(block_num.into()));
    let pairs = triangular_pairs(cfg);
    let min_liq = crate::dex::liquidity::min_pool_liquidity_usd(cfg);
    let mut out: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let qs = Address::from_str(QUICKSWAP_ROUTER)?;
    let sushi = Address::from_str(SUSHISWAP_ROUTER)?;

    for (a, b) in &pairs {
        match quoter.quote_v3_best(a, b, block).await {
            Ok(Some((price, fee))) => {
                let keep = match (
                    quoter.resolve(a).await,
                    quoter.resolve(b).await,
                    crate::dex::liquidity::cached_pool_address("UniswapV3", a, b, fee),
                ) {
                    (Ok((aa, da)), Ok((bb, db)), Some(pool)) => {
                        let pa = crate::infra::price_feed::PRICE_FEED
                            .get_price(a)
                            .await
                            .unwrap_or(0.0);
                        let pb = crate::infra::price_feed::PRICE_FEED
                            .get_price(b)
                            .await
                            .unwrap_or(0.0);
                        match quoter
                            .pool_tvl_usd(pool, aa, da, pa, bb, db, pb, block)
                            .await
                        {
                            Ok(tvl) => {
                                let pass =
                                    crate::dex::liquidity::passes_liquidity_gate(tvl, min_liq);
                                if !pass {
                                    crate::dex::liquidity::note_low_liquidity_discarded_pub(1);
                                }
                                pass
                            }
                            Err(e)
                                if paper_validation::is_archive_state_error(&e.to_string()) =>
                            {
                                return Err(e);
                            }
                            Err(_) => true,
                        }
                    }
                    _ => true,
                };
                if keep {
                    out.entry("UniswapV3".into())
                        .or_default()
                        .insert(format!("{a}-{b}"), price);
                }
            }
            Ok(None) => {}
            Err(e) => return Err(e),
        }

        for (dex, router) in [("QuickSwap", qs), ("SushiSwap", sushi)] {
            match quoter.quote_v2(dex, router, a, b, block).await {
                Ok(Some(price)) => {
                    out.entry(dex.into())
                        .or_default()
                        .insert(format!("{a}-{b}"), price);
                }
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
    }
    Ok(out)
}

fn best_tri_from_opps(opps: &[ArbitrageOpportunity]) -> Option<(f64, &ArbitrageOpportunity)> {
    opps.iter()
        .filter(|o| o.steps.0.len() >= 3)
        .map(|o| {
            let rate = 1.0 + (o.spread_percent / 100.0);
            (rate, o)
        })
        .max_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal))
}

fn route_and_fees(opp: &ArbitrageOpportunity) -> (String, String) {
    let route = opp
        .steps
        .0
        .iter()
        .map(|s| format!("{}:{}→{}", s.dex_name, s.token_in, s.token_out))
        .collect::<Vec<_>>()
        .join("|");
    let fees = opp
        .steps
        .0
        .iter()
        .map(|s| {
            s.v3_fee_tier
                .map(|f| f.to_string())
                .unwrap_or_else(|| "-".into())
        })
        .collect::<Vec<_>>()
        .join(";");
    (route, fees)
}

fn append_csv(path: &Path, sample: &ReplayBlockSample, header: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if header {
        writeln!(
            f,
            "block,quote_block,eth_call_block,best_cycle_rate,fee_floor,edge_exists,pair,route,fee_tiers,net_previsto_usd,profit_realizado_usd,erro_rel_pct,sim_ok,revert_reason,archive_error"
        )?;
    }
    writeln!(
        f,
        "{},{},{},{:.10},{:.10},{},{},{},{},{},{},{},{},{},{}",
        sample.block,
        sample.quote_block,
        sample
            .eth_call_block
            .map(|b| b.to_string())
            .unwrap_or_default(),
        sample.best_cycle_rate,
        sample.fee_floor,
        sample.edge_exists,
        sample.pair,
        sample.route.replace(',', ";"),
        sample.fee_tiers,
        sample
            .net_previsto_usd
            .map(|v| format!("{v:.6}"))
            .unwrap_or_default(),
        sample
            .profit_realizado_usd
            .map(|v| format!("{v:.6}"))
            .unwrap_or_default(),
        sample
            .erro_rel_pct
            .map(|v| format!("{v:.4}"))
            .unwrap_or_default(),
        sample
            .sim_ok
            .map(|v| v.to_string())
            .unwrap_or_default(),
        sample
            .revert_reason
            .as_deref()
            .unwrap_or("")
            .replace(',', ";"),
        sample
            .archive_error
            .as_deref()
            .unwrap_or("")
            .replace(',', ";"),
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Main entry
// ---------------------------------------------------------------------------

pub async fn run(
    client: Arc<AppMiddleware>,
    cfg: Arc<Config>,
    engine: &ArbitrageEngine,
    arb_client: &ArbitrageClient,
) -> Result<ReplayScanSummary> {
    if !replay_scan_allowed(&cfg) {
        bail!("replay_scan: blocked (need observation_active + REPLAY_SCAN/config)");
    }
    if !paper_validation::sends_forbidden(&cfg) {
        bail!("replay_scan: sends_forbidden required");
    }

    let latest = client.get_block_number().await?.as_u64();
    let latest_block = client
        .get_block(BlockId::Number(BlockNumber::Number(latest.into())))
        .await?
        .ok_or_else(|| anyhow!("latest block missing"))?;
    let latest_ts = latest_block.timestamp.as_u64();

    let replay = &cfg.validation.replay;
    let (from, to, label) = resolve_block_range(replay, latest, latest_ts)?;
    let step = replay.step.max(1);
    let max_eth = replay.max_eth_calls;

    info!(
        target: "replay_scan",
        %label,
        from,
        to,
        step,
        max_eth,
        "📼 REPLAY SCAN start (paper-only, archive required)"
    );

    let quoter = HistQuoter::new(client.clone(), cfg.clone()).await?;
    let csv_path = Path::new(&replay.csv_path);
    let _ = std::fs::remove_file(csv_path);

    let mut discovery = (*cfg).clone();
    // Scan precisa ver cycle_rate abaixo de 1.0 (piso de fee) — floor negativo
    // só na discovery desta corrida; não altera config de execução.
    discovery.arbitrage.min_spread_percent = "-50.0".into();
    discovery.arbitrage.min_profit_threshold_usd = Some(-1.0e9);

    let mut samples = Vec::new();
    let mut eth_calls = 0u64;
    let mut first_csv = true;

    let mut block = from;
    while block <= to {
        let mut sample = ReplayBlockSample {
            block,
            quote_block: block,
            eth_call_block: None,
            best_cycle_rate: 0.0,
            fee_floor: 0.0,
            edge_exists: false,
            pair: String::new(),
            route: String::new(),
            fee_tiers: String::new(),
            net_previsto_usd: None,
            profit_realizado_usd: None,
            erro_rel_pct: None,
            sim_ok: None,
            revert_reason: None,
            archive_error: None,
        };

        match build_price_map_at_block(&quoter, &cfg, block).await {
            Ok(prices) => {
                let opps = engine
                    .find_arbitrage_opportunities(&prices, &discovery)
                    .await;
                if let Some((rate, opp)) = best_tri_from_opps(&opps) {
                    let (route, fees) = route_and_fees(opp);
                    sample.best_cycle_rate = rate;
                    sample.fee_floor = theoretical_fee_floor(&opp.steps.0);
                    sample.edge_exists = edge_exists(rate);
                    sample.pair = opp.pair.clone();
                    sample.route = route;
                    sample.fee_tiers = fees;
                    sample.net_previsto_usd = Some(opp.net_profit_usd);

                    if sample.edge_exists && eth_calls < max_eth {
                        assert_same_block_tag(sample.quote_block, block)?;
                        let slip = cfg.flashloan.slippage_bps.unwrap_or(15) as u64;
                        let fl_fee = if cfg.flashloan.enabled {
                            opp.estimated_volume_usd * cfg.flashloan.fee_pct.unwrap_or(0.0005)
                        } else {
                            0.0
                        };
                        let would = paper_validation::would_execute(opp.spread_percent, &cfg);
                        match arb_client
                            .paper_validate_at_block(opp, block, slip, fl_fee, would)
                            .await
                        {
                            Ok(ps) => {
                                eth_calls += 1;
                                sample.eth_call_block = Some(ps.block_number);
                                let _ = assert_same_block_tag(sample.quote_block, ps.block_number);
                                sample.sim_ok = Some(ps.sim_ok);
                                sample.profit_realizado_usd = ps.profit_realizado_usd;
                                sample.erro_rel_pct = ps.erro_rel_pct;
                                sample.revert_reason = ps.revert_reason;
                                sample.net_previsto_usd = Some(ps.net_previsto_usd);
                            }
                            Err(e) => {
                                let s = e.to_string();
                                if paper_validation::is_archive_state_error(&s) {
                                    sample.archive_error = Some(s);
                                    warn!(target: "replay_scan", block, "archive abort");
                                } else {
                                    sample.revert_reason = Some(s);
                                }
                            }
                        }
                    }
                }
            }
            Err(e) => {
                let s = e.to_string();
                if paper_validation::is_archive_state_error(&s) {
                    sample.archive_error = Some(s);
                    warn!(target: "replay_scan", block, "archive abort on quote");
                } else {
                    warn!(target: "replay_scan", block, error = %s, "quote failed");
                    sample.revert_reason = Some(s);
                }
            }
        }

        if let Err(e) = append_csv(csv_path, &sample, first_csv) {
            warn!(target: "replay_scan", "csv: {e}");
        }
        first_csv = false;
        samples.push(sample);
        block = block.saturating_add(step);
    }

    let summary = ReplayScanSummary::from_samples(&samples, from, to, step);
    summary.log();
    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::core::types::ArbitrageStep;

    #[test]
    fn same_block_tag_ok() {
        assert!(assert_same_block_tag(100, 100).is_ok());
        assert!(assert_same_block_tag(100, 101).is_err());
    }

    #[test]
    fn archive_error_detected() {
        assert!(paper_validation::is_archive_state_error(
            "missing trie node 0xabc"
        ));
        assert!(paper_validation::is_archive_state_error(
            "historical state is not available"
        ));
        assert!(!paper_validation::is_archive_state_error("execution reverted"));
    }

    #[test]
    fn scan_requires_paper_observation() {
        let mut cfg = Config::default();
        cfg.validation.paper_enabled = false;
        cfg.validation.dry_run_only = false;
        cfg.execution.dry_run = false;
        cfg.validation.replay.enabled = true;
        std::env::remove_var(ENV_REPLAY_SCAN);
        assert!(!replay_scan_allowed(&cfg));

        cfg.validation.paper_enabled = true;
        assert!(replay_scan_allowed(&cfg));
        assert!(paper_validation::sends_forbidden(&cfg));
    }

    #[test]
    fn fee_floor_three_v3_hops() {
        let steps = vec![
            ArbitrageStep {
                dex_name: "UniswapV3".into(),
                v3_fee_tier: Some(500),
                ..Default::default()
            },
            ArbitrageStep {
                dex_name: "UniswapV3".into(),
                v3_fee_tier: Some(3000),
                ..Default::default()
            },
            ArbitrageStep {
                dex_name: "UniswapV3".into(),
                v3_fee_tier: Some(500),
                ..Default::default()
            },
        ];
        let floor = theoretical_fee_floor(&steps);
        let expected = (1.0 - 0.0005) * (1.0 - 0.003) * (1.0 - 0.0005);
        assert!((floor - expected).abs() < 1e-12);
    }

    #[test]
    fn edge_exists_gt_one() {
        assert!(!edge_exists(0.997));
        assert!(!edge_exists(1.0));
        assert!(edge_exists(1.0001));
    }

    #[test]
    fn us_session_window_weekday() {
        let latest_ts = Utc
            .with_ymd_and_hms(2026, 7, 24, 18, 0, 0)
            .unwrap()
            .timestamp() as u64;
        let latest_block = 75_000_000u64;
        let (from, to, label) = resolve_us_session_window(latest_block, latest_ts);
        assert!(from < to);
        assert!(label.contains("14:30"));
        assert!(to - from > 1000);
    }
}
