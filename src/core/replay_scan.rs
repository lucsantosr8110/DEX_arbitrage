//! Replay scan denso: janelas contíguas (`step=1`), multicall@blockTag, paper-only.
//!
//! Quote e eth_call usam o **mesmo** `blockTag`. Envio permanece bloqueado.
//! Não altera o finder — só alimenta price_map@block.

use crate::{
    config::{token_cache::TokenCache, Config, ReplayConfig},
    core::{
        arbitrage::ArbitrageEngine,
        flashloan::ArbitrageClient,
        paper_validation,
        replay_cross_model::{
            self, find_best_stable_cycle, should_eth_call_for_route, validate_stable_decimals,
            QuoteIndex, STABLE_SYMBOLS,
        },
        types::{ArbitrageOpportunity, ArbitrageStep},
    },
    dex::{
        cache_fee_tier, calculate_price_from_decimals,
        liquidity::{
            min_pool_liquidity_usd, note_low_liquidity_discarded_pub, passes_liquidity_gate,
            pool_tvl_usd_from_balances,
        },
        quote_amount_for_usd, rate_limiter::ALCHEMY_RATE_LIMITER, select_executable_v3_best_out,
        EXECUTABLE_V3_FEE_TIERS,
    },
    AppMiddleware,
};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, Duration as ChronoDuration, TimeZone, Utc, Weekday};
use ethers::{
    abi::{Abi, Token},
    contract::{Contract, Multicall},
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
    time::{Duration, Instant},
};
use tracing::{info, warn};

pub const ENV_REPLAY_SCAN: &str = "REPLAY_SCAN";

/// ~2.0s/bloco Polygon (estimativa timestamp→block).
const POLYGON_BLOCK_SECS: f64 = 2.0;

const V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const QUICKSWAP_ROUTER: &str = "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff";
const SUSHISWAP_ROUTER: &str = "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506";
/// Curve Aave plain pool (amDAI/amUSDC/amUSDT) — Polygon.
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";

const MULTICALL_BATCH_PAIRS: usize = 12;
const THROUGHPUT_LOG_EVERY: u64 = 50;

const CURVE_GET_DY_ABI: &str = r#"[
  {"name":"get_dy","outputs":[{"type":"uint256","name":""}],
   "inputs":[{"type":"int128","name":"i"},{"type":"int128","name":"j"},{"type":"uint256","name":"dx"}],
   "stateMutability":"view","type":"function"},
  {"name":"balances","outputs":[{"type":"uint256","name":""}],
   "inputs":[{"type":"uint256","name":"i"}],
   "stateMutability":"view","type":"function"}
]"#;

// ---------------------------------------------------------------------------
// Gate / helpers
// ---------------------------------------------------------------------------

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

pub fn theoretical_fee_floor(steps: &[ArbitrageStep]) -> f64 {
    let mut p = 1.0_f64;
    for s in steps {
        p *= 1.0 - replay_cross_model::venue_fee_fraction(&s.dex_name, s.v3_fee_tier);
    }
    p
}

pub fn edge_exists(cycle_rate: f64) -> bool {
    cycle_rate.is_finite() && cycle_rate > 1.0
}

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

/// eth_call só em edge e sob o cap global de simulações.
pub fn should_eth_call(edge: bool, eth_calls_so_far: u64, max_eth_calls: u64) -> bool {
    edge && eth_calls_so_far < max_eth_calls
}

/// Blocos amostrados em `[from, to]` com `step` (inclusive).
pub fn sample_block_list(from: u64, to: u64, step: u64) -> Vec<u64> {
    let step = step.max(1);
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    let mut out = Vec::new();
    let mut b = lo;
    while b <= hi {
        out.push(b);
        b = b.saturating_add(step);
    }
    out
}

pub fn expected_contiguous_count(from: u64, to: u64) -> u64 {
    let (lo, hi) = if from <= to { (from, to) } else { (to, from) };
    hi.saturating_sub(lo).saturating_add(1)
}

// ---------------------------------------------------------------------------
// Histogram
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, Serialize)]
pub struct CycleRateHistogram {
    pub lt_0_99: u64,
    pub b_0_99_0_995: u64,
    pub b_0_995_1_0: u64,
    pub gte_1_0: u64,
}

impl CycleRateHistogram {
    pub fn from_rates(rates: &[f64]) -> Self {
        let mut h = Self::default();
        for &r in rates {
            if !r.is_finite() || r <= 0.0 {
                continue;
            }
            if r < 0.99 {
                h.lt_0_99 += 1;
            } else if r < 0.995 {
                h.b_0_99_0_995 += 1;
            } else if r < 1.0 {
                h.b_0_995_1_0 += 1;
            } else {
                h.gte_1_0 += 1;
            }
        }
        h
    }

    pub fn merge(&mut self, other: &Self) {
        self.lt_0_99 += other.lt_0_99;
        self.b_0_99_0_995 += other.b_0_99_0_995;
        self.b_0_995_1_0 += other.b_0_995_1_0;
        self.gte_1_0 += other.gte_1_0;
    }
}

// ---------------------------------------------------------------------------
// Sample + summaries
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct ReplayBlockSample {
    pub window_label: String,
    pub block: u64,
    pub quote_block: u64,
    pub eth_call_block: Option<u64>,
    /// Best same-model (CPMM-only midcap finder) cycle_rate.
    pub same_model_rate: f64,
    /// Best cross-model (Curve×CPMM stables) cycle_rate.
    pub cross_model_rate: f64,
    pub cross_model: bool,
    pub quote_only: bool,
    pub best_cycle_rate: f64,
    pub fee_floor: f64,
    pub edge_exists: bool,
    pub edge_same_model: bool,
    pub edge_cross_model: bool,
    pub pair: String,
    pub route: String,
    pub fee_tiers: String,
    pub models: String,
    pub net_previsto_usd: Option<f64>,
    pub profit_realizado_usd: Option<f64>,
    pub erro_rel_pct: Option<f64>,
    pub sim_ok: Option<bool>,
    pub revert_reason: Option<String>,
    pub archive_error: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ModelGroupSummary {
    pub n_with_rate: u64,
    pub best_cycle_rate_min: Option<f64>,
    pub best_cycle_rate_p50: Option<f64>,
    pub best_cycle_rate_p95: Option<f64>,
    pub best_cycle_rate_max: Option<f64>,
    pub histogram: CycleRateHistogram,
    pub n_blocos_com_edge: u64,
    pub n_blocos_lucrativos_pos_custos: u64,
    pub n_reached_eth_call: u64,
    pub n_sim_ok: u64,
    pub n_reverts: u64,
    pub n_quote_only_edges: u64,
    pub erro_rel_pct_p50: Option<f64>,
    pub erro_rel_pct_p95: Option<f64>,
    pub top10: Vec<(u64, f64, String)>,
}

impl ModelGroupSummary {
    fn from_rates_and_meta(
        rates: &[f64],
        n_edge: u64,
        n_prof: u64,
        n_eth: u64,
        n_ok: u64,
        n_reverts: u64,
        n_quote_only_edges: u64,
        errs: &[f64],
        top10: Vec<(u64, f64, String)>,
    ) -> Self {
        let mut sorted = rates.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut errs_s = errs.to_vec();
        errs_s.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        Self {
            n_with_rate: sorted.len() as u64,
            best_cycle_rate_min: sorted.first().copied(),
            best_cycle_rate_p50: percentile(&sorted, 50.0),
            best_cycle_rate_p95: percentile(&sorted, 95.0),
            best_cycle_rate_max: sorted.last().copied(),
            histogram: CycleRateHistogram::from_rates(&sorted),
            n_blocos_com_edge: n_edge,
            n_blocos_lucrativos_pos_custos: n_prof,
            n_reached_eth_call: n_eth,
            n_sim_ok: n_ok,
            n_reverts,
            n_quote_only_edges,
            erro_rel_pct_p50: percentile(&errs_s, 50.0),
            erro_rel_pct_p95: percentile(&errs_s, 95.0),
            top10,
        }
    }

    fn log(&self, prefix: &str, group: &str) {
        info!(
            target: "replay_scan",
            "📊 {} {} | n_rate={} rate min={:?} p50={:?} p95={:?} max={:?} | hist[<0.99={},0.99-0.995={},0.995-1.0={},>=1.0={}] | n_edge={} n_lucrativos={} | eth_call={} sim_ok={} reverts={} quote_only_edges={} | erro_rel% p50={:?} p95={:?}",
            prefix,
            group,
            self.n_with_rate,
            self.best_cycle_rate_min,
            self.best_cycle_rate_p50,
            self.best_cycle_rate_p95,
            self.best_cycle_rate_max,
            self.histogram.lt_0_99,
            self.histogram.b_0_99_0_995,
            self.histogram.b_0_995_1_0,
            self.histogram.gte_1_0,
            self.n_blocos_com_edge,
            self.n_blocos_lucrativos_pos_custos,
            self.n_reached_eth_call,
            self.n_sim_ok,
            self.n_reverts,
            self.n_quote_only_edges,
            self.erro_rel_pct_p50,
            self.erro_rel_pct_p95,
        );
        for (i, (b, r, p)) in self.top10.iter().enumerate() {
            info!(
                target: "replay_scan",
                "📊 {} {} TOP{} block={} rate={:.8} {}",
                prefix,
                group,
                i + 1,
                b,
                r,
                p
            );
        }
    }
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct ReplayScanSummary {
    pub label: String,
    pub profile: String,
    pub n_blocos_amostrados: u64,
    pub n_blocos_range: u64,
    pub contiguous_full: bool,
    pub block_from: u64,
    pub block_to: u64,
    pub step: u64,
    pub curve_executable: bool,
    pub curve_mode: String,
    pub same_model: ModelGroupSummary,
    pub cross_model: ModelGroupSummary,
    /// Legacy combined fields (best overall rate for backward logs).
    pub best_cycle_rate_min: Option<f64>,
    pub best_cycle_rate_p50: Option<f64>,
    pub best_cycle_rate_p95: Option<f64>,
    pub best_cycle_rate_max: Option<f64>,
    pub histogram: CycleRateHistogram,
    pub n_blocos_com_edge: u64,
    pub n_blocos_lucrativos_pos_custos: u64,
    pub n_reached_eth_call: u64,
    pub n_sim_ok: u64,
    pub n_reverts: u64,
    pub revert_reasons: Vec<(String, u64)>,
    pub n_archive_abort: u64,
    pub erro_rel_pct_p50: Option<f64>,
    pub erro_rel_pct_p95: Option<f64>,
    pub top10: Vec<(u64, f64, String)>,
    pub throughput_blocks_per_sec: Option<f64>,
    pub elapsed_secs: Option<f64>,
}

impl ReplayScanSummary {
    pub fn from_samples(
        samples: &[ReplayBlockSample],
        from: u64,
        to: u64,
        step: u64,
        label: &str,
        profile: &str,
        elapsed: Option<Duration>,
    ) -> Self {
        let same_rates: Vec<f64> = samples
            .iter()
            .map(|s| s.same_model_rate)
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();
        let cross_rates: Vec<f64> = samples
            .iter()
            .map(|s| s.cross_model_rate)
            .filter(|r| r.is_finite() && *r > 0.0)
            .collect();

        let mut same_top: Vec<(u64, f64, String)> = samples
            .iter()
            .filter(|s| s.same_model_rate > 0.0)
            .map(|s| {
                (
                    s.block,
                    s.same_model_rate,
                    format!("{} | {}", s.pair, s.route),
                )
            })
            .collect();
        same_top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        same_top.truncate(10);

        let mut cross_top: Vec<(u64, f64, String)> = samples
            .iter()
            .filter(|s| s.cross_model && s.cross_model_rate > 0.0)
            .map(|s| {
                (
                    s.block,
                    s.cross_model_rate,
                    format!("{} | {} | models={}", s.pair, s.route, s.models),
                )
            })
            .collect();
        cross_top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        cross_top.truncate(10);

        let same_errs: Vec<f64> = samples
            .iter()
            .filter(|s| !s.cross_model && s.sim_ok == Some(true))
            .filter_map(|s| s.erro_rel_pct)
            .collect();
        // cross-model is quote-only → no erro_rel from eth_call
        let cross_errs: Vec<f64> = Vec::new();

        let same = ModelGroupSummary::from_rates_and_meta(
            &same_rates,
            samples.iter().filter(|s| s.edge_same_model).count() as u64,
            samples
                .iter()
                .filter(|s| {
                    !s.quote_only
                        && s.sim_ok == Some(true)
                        && s.profit_realizado_usd.map(|p| p > 0.0).unwrap_or(false)
                })
                .count() as u64,
            samples
                .iter()
                .filter(|s| s.eth_call_block.is_some() && !s.quote_only)
                .count() as u64,
            samples
                .iter()
                .filter(|s| s.sim_ok == Some(true) && !s.quote_only)
                .count() as u64,
            samples
                .iter()
                .filter(|s| s.sim_ok == Some(false) && !s.quote_only)
                .count() as u64,
            0,
            &same_errs,
            same_top.clone(),
        );

        let cross = ModelGroupSummary::from_rates_and_meta(
            &cross_rates,
            samples.iter().filter(|s| s.edge_cross_model).count() as u64,
            0, // quote-only → no lucro pos eth_call
            0,
            0,
            0,
            samples
                .iter()
                .filter(|s| s.edge_cross_model && s.quote_only)
                .count() as u64,
            &cross_errs,
            cross_top.clone(),
        );

        let same_err_p50 = same.erro_rel_pct_p50;
        let same_err_p95 = same.erro_rel_pct_p95;

        let mut rates = same_rates.clone();
        rates.extend(cross_rates.iter().copied());
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let n_arch = samples.iter().filter(|s| s.archive_error.is_some()).count() as u64;
        let mut reason_counts: HashMap<String, u64> = HashMap::new();
        let mut n_reverts = 0u64;
        for s in samples {
            if s.sim_ok == Some(false) {
                n_reverts += 1;
                if let Some(r) = &s.revert_reason {
                    *reason_counts
                        .entry(r.chars().take(120).collect())
                        .or_default() += 1;
                }
            }
        }
        let mut revert_reasons: Vec<(String, u64)> = reason_counts.into_iter().collect();
        revert_reasons.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        revert_reasons.truncate(10);

        let n_range = expected_contiguous_count(from, to);
        let n_samp = samples.len() as u64;
        let contiguous_full = step <= 1 && n_samp == n_range;
        let elapsed_secs = elapsed.map(|d| d.as_secs_f64());
        let throughput = elapsed_secs.and_then(|s| {
            if s > 0.0 {
                Some(n_samp as f64 / s)
            } else {
                None
            }
        });

        let mut top = same_top;
        top.extend(cross_top);
        top.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        top.truncate(10);

        Self {
            label: label.to_string(),
            profile: profile.to_string(),
            n_blocos_amostrados: n_samp,
            n_blocos_range: n_range,
            contiguous_full,
            block_from: from.min(to),
            block_to: from.max(to),
            step,
            curve_executable: replay_cross_model::curve_executor_supported(),
            curve_mode: if replay_cross_model::curve_executor_supported() {
                "executable".into()
            } else {
                "QUOTE_ONLY".into()
            },
            same_model: same,
            cross_model: cross,
            best_cycle_rate_min: rates.first().copied(),
            best_cycle_rate_p50: percentile(&rates, 50.0),
            best_cycle_rate_p95: percentile(&rates, 95.0),
            best_cycle_rate_max: rates.last().copied(),
            histogram: CycleRateHistogram::from_rates(&rates),
            n_blocos_com_edge: samples.iter().filter(|s| s.edge_exists).count() as u64,
            n_blocos_lucrativos_pos_custos: samples
                .iter()
                .filter(|s| {
                    s.sim_ok == Some(true)
                        && s.profit_realizado_usd.map(|p| p > 0.0).unwrap_or(false)
                })
                .count() as u64,
            n_reached_eth_call: samples.iter().filter(|s| s.eth_call_block.is_some()).count()
                as u64,
            n_sim_ok: samples.iter().filter(|s| s.sim_ok == Some(true)).count() as u64,
            n_reverts,
            revert_reasons,
            n_archive_abort: n_arch,
            erro_rel_pct_p50: same_err_p50,
            erro_rel_pct_p95: same_err_p95,
            top10: top,
            throughput_blocks_per_sec: throughput,
            elapsed_secs,
        }
    }

    pub fn log(&self, prefix: &str) {
        info!(
            target: "replay_scan",
            "📊 {} SUMMARY | label={} profile={} sampled={} range_size={} contiguous_full={} range=[{}..{}] step={} | curve={} ({}) | thrpt={:?} blk/s elapsed={:?}",
            prefix,
            self.label,
            self.profile,
            self.n_blocos_amostrados,
            self.n_blocos_range,
            self.contiguous_full,
            self.block_from,
            self.block_to,
            self.step,
            self.curve_mode,
            replay_cross_model::curve_execution_diagnostic(),
            self.throughput_blocks_per_sec,
            self.elapsed_secs,
        );
        self.same_model.log(prefix, "SAME_MODEL");
        self.cross_model.log(prefix, "CROSS_MODEL");
        for (reason, n) in &self.revert_reasons {
            info!(
                target: "replay_scan",
                "📊 {} REVERT | n={} reason={}",
                prefix,
                n,
                reason
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
// Block range / windows
// ---------------------------------------------------------------------------

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

/// Resolve lista de janelas. `windows` no config tem prioridade.
pub fn resolve_windows(
    cfg: &ReplayConfig,
    latest_block: u64,
    latest_ts: u64,
) -> Result<Vec<(u64, u64, String, String)>> {
    if !cfg.windows.is_empty() {
        let mut out = Vec::new();
        for w in &cfg.windows {
            if w.from == 0 && w.to == 0 {
                continue;
            }
            let (lo, hi) = if w.from <= w.to {
                (w.from, w.to)
            } else {
                (w.to, w.from)
            };
            let label = if w.label.is_empty() {
                format!("window_{lo}_{hi}")
            } else {
                w.label.clone()
            };
            out.push((lo, hi, label, w.profile.clone()));
        }
        if out.is_empty() {
            bail!("replay: windows presentes mas vazias/zeradas");
        }
        return Ok(out);
    }

    let from = cfg.block_from.filter(|&b| b > 0);
    let to = cfg.block_to.filter(|&b| b > 0);
    match (from, to) {
        (Some(a), Some(b)) => {
            let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
            Ok(vec![(
                lo,
                hi,
                format!("config range [{lo}..{hi}]"),
                "legacy".into(),
            )])
        }
        _ if cfg.auto_us_session => {
            let (lo, hi, label) = resolve_us_session_window(latest_block, latest_ts);
            Ok(vec![(lo, hi, label, "auto_us_session".into())])
        }
        _ => bail!("replay: windows/block_from/to ausentes e auto_us_session=false"),
    }
}

// ---------------------------------------------------------------------------
// Historical quotes (multicall@block)
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

struct PairMeta {
    token_a: String,
    token_b: String,
    addr_a: Address,
    addr_b: Address,
    dec_a: u8,
    dec_b: u8,
    amount_in: U256,
}

struct HistQuoter {
    client: Arc<AppMiddleware>,
    token_cache: Arc<TokenCache>,
    quoter_abi: Abi,
    router_abi: Abi,
    curve_abi: Abi,
    notional_usd: f64,
    min_interval: Duration,
    /// B2 gate real (balances() @ latest block) — false ⇒ Curve leg skip, sem revert silencioso.
    curve_liquid: bool,
}

impl HistQuoter {
    async fn new(client: Arc<AppMiddleware>, cfg: Arc<Config>) -> Result<Self> {
        let quoter_abi = parse_abi_flexible(include_str!("../../abi/uniswap_v3_quoter.json"))
            .context("parse uniswap_v3_quoter abi")?;
        let router_abi = parse_abi_flexible(include_str!("../../abi/uniswap_v2_router.json"))
            .context("parse uniswap_v2_router abi")?;
        let curve_abi: Abi =
            serde_json::from_str(CURVE_GET_DY_ABI).context("parse curve get_dy abi")?;
        let token_cache = TokenCache::global(cfg.clone()).await;
        let min_interval = Duration::from_millis(cfg.validation.replay.min_interval_ms);
        Ok(Self {
            client,
            token_cache,
            quoter_abi,
            router_abi,
            curve_abi,
            notional_usd: 100.0,
            min_interval,
            curve_liquid: false,
        })
    }

    /// B2 gate real: soma `balances(i)` do pool Curve Aave @ `block_num`, compara
    /// contra `min_pool_liquidity_usd(cfg)`. Stables ≈ $1 (mesmo fallback de
    /// `dex::liquidity::token_price_usd`). Chamado 1x no boot do scan, não por bloco.
    async fn probe_curve_liquidity(&self, cfg: &Config, block_num: u64) -> Result<(f64, bool)> {
        let block = BlockId::Number(BlockNumber::Number(block_num.into()));
        let pool = Contract::new(
            Address::from_str(CURVE_AAVE_POOL)?,
            self.curve_abi.clone(),
            self.client.clone(),
        );
        // Índices do pool: DAI=0 (18 dec), USDC=1 (6 dec), USDT=2 (6 dec).
        let mut bal = [U256::zero(); 3];
        for (i, b) in bal.iter_mut().enumerate() {
            self.pace().await;
            *b = pool
                .method::<_, U256>("balances", U256::from(i as u64))?
                .block(block)
                .call()
                .await
                .map_err(|e| anyhow!("curve balances({i}) failed: {e}"))?;
        }
        let tvl = pool_tvl_usd_from_balances(bal[0], 18, 1.0, bal[1], 6, 1.0)
            + pool_tvl_usd_from_balances(bal[2], 6, 1.0, U256::zero(), 0, 0.0);
        let min_usd = min_pool_liquidity_usd(cfg);
        let ok = passes_liquidity_gate(tvl, min_usd);
        Ok((tvl, ok))
    }

    /// Abort se USDC/USDT/DAI decimals ≠ canônico.
    async fn assert_stable_decimals(&self) -> Result<()> {
        for sym in STABLE_SYMBOLS {
            let (_, dec) = self.resolve(sym).await?;
            validate_stable_decimals(sym, dec)?;
        }
        Ok(())
    }

    async fn pace(&self) {
        let _ = ALCHEMY_RATE_LIMITER.acquire().await;
        if !self.min_interval.is_zero() {
            tokio::time::sleep(self.min_interval).await;
        }
    }

    async fn resolve(&self, symbol: &str) -> Result<(Address, u8)> {
        let info = self
            .token_cache
            .get_by_symbol(symbol)
            .await
            .ok_or_else(|| anyhow!("token_cache miss: {symbol}"))?;
        Ok((info.address, info.decimals))
    }

    async fn prepare_pairs(&self, pairs: &[(String, String)]) -> Result<Vec<PairMeta>> {
        let mut out = Vec::with_capacity(pairs.len());
        for (a, b) in pairs {
            let (addr_a, dec_a) = self.resolve(a).await?;
            let (addr_b, dec_b) = self.resolve(b).await?;
            let amount_in = quote_amount_for_usd(a, dec_a, self.notional_usd).await?;
            out.push(PairMeta {
                token_a: a.clone(),
                token_b: b.clone(),
                addr_a,
                addr_b,
                dec_a,
                dec_b,
                amount_in,
            });
        }
        Ok(out)
    }

    async fn quote_v3_multicall(
        &self,
        metas: &[PairMeta],
        block_num: u64,
    ) -> Result<HashMap<(String, String), (f64, u32)>> {
        let block = BlockNumber::Number(block_num.into());
        let quoter = Contract::new(
            Address::from_str(V3_QUOTER)?,
            self.quoter_abi.clone(),
            self.client.clone(),
        );
        let n_fees = EXECUTABLE_V3_FEE_TIERS.len();
        let mut results = HashMap::new();

        for batch in metas.chunks(MULTICALL_BATCH_PAIRS) {
            let mut multicall = Multicall::new(self.client.clone(), None).await?;
            multicall = multicall.block(block);
            for m in batch {
                for &fee in &EXECUTABLE_V3_FEE_TIERS {
                    let call = quoter.method::<_, U256>(
                        "quoteExactInputSingle",
                        (m.addr_a, m.addr_b, fee, m.amount_in, U256::zero()),
                    )?;
                    multicall.add_call(call, true);
                }
            }
            self.pace().await;
            let raw: Vec<Result<Token, _>> = match multicall.call_raw().await {
                Ok(r) => r,
                Err(e) => {
                    let s = e.to_string();
                    if paper_validation::is_archive_state_error(&s) {
                        return Err(anyhow!("archive state unavailable: {s}"));
                    }
                    warn!(target: "replay_scan", block = block_num, error = %s, "v3 multicall batch fail");
                    continue;
                }
            };

            for (i, chunk) in raw.chunks_exact(n_fees).enumerate() {
                let m = &batch[i];
                let mut quotes: Vec<(u32, U256)> = Vec::new();
                for (j, r) in chunk.iter().enumerate() {
                    if let Ok(Token::Uint(v)) = r {
                        if !v.is_zero() {
                            quotes.push((EXECUTABLE_V3_FEE_TIERS[j], *v));
                        }
                    }
                }
                let Some((best_fee, best_out)) = select_executable_v3_best_out(&quotes) else {
                    continue;
                };
                let price = calculate_price_from_decimals(
                    m.amount_in,
                    best_out,
                    m.dec_a,
                    m.dec_b,
                )?;
                if price > 0.0 {
                    cache_fee_tier("UniswapV3", &m.token_a, &m.token_b, best_fee);
                    results.insert((m.token_a.clone(), m.token_b.clone()), (price, best_fee));
                }
            }
        }
        Ok(results)
    }

    async fn quote_v2_multicall(
        &self,
        dex: &str,
        router: Address,
        metas: &[PairMeta],
        block_num: u64,
    ) -> Result<HashMap<(String, String), f64>> {
        let block = BlockNumber::Number(block_num.into());
        let router_c = Contract::new(router, self.router_abi.clone(), self.client.clone());
        let mut results = HashMap::new();

        for batch in metas.chunks(MULTICALL_BATCH_PAIRS * 2) {
            let mut multicall = Multicall::new(self.client.clone(), None).await?;
            multicall = multicall.block(block);
            for m in batch {
                let call = router_c.method::<_, Vec<U256>>(
                    "getAmountsOut",
                    (m.amount_in, vec![m.addr_a, m.addr_b]),
                )?;
                multicall.add_call(call, true);
            }
            self.pace().await;
            let raw: Vec<Result<Token, _>> = match multicall.call_raw().await {
                Ok(r) => r,
                Err(e) => {
                    let s = e.to_string();
                    if paper_validation::is_archive_state_error(&s) {
                        return Err(anyhow!("archive state unavailable ({dex}): {s}"));
                    }
                    warn!(target: "replay_scan", block = block_num, dex, error = %s, "v2 multicall batch fail");
                    continue;
                }
            };
            for (i, r) in raw.into_iter().enumerate() {
                let m = &batch[i];
                let Ok(Token::Array(arr)) = r else {
                    continue;
                };
                if arr.len() < 2 {
                    continue;
                }
                let Token::Uint(out) = &arr[1] else {
                    continue;
                };
                if out.is_zero() {
                    continue;
                }
                let price =
                    calculate_price_from_decimals(m.amount_in, *out, m.dec_a, m.dec_b)?;
                if price > 0.0 {
                    results.insert((m.token_a.clone(), m.token_b.clone()), price);
                }
            }
        }
        Ok(results)
    }

    /// Curve Aave pool get_dy@block — sequencial. Índices: DAI=0, USDC=1, USDT=2.
    async fn quote_curve_stables(
        &self,
        block_num: u64,
    ) -> Result<HashMap<(String, String), f64>> {
        if !self.curve_liquid {
            // B2 gate falhou no probe inicial — sem quote-only silencioso: cross_model
            // simplesmente não aparece nesta janela (sem eth_call, sem fake edge).
            return Ok(HashMap::new());
        }
        let block = BlockId::Number(BlockNumber::Number(block_num.into()));
        let pool = Contract::new(
            Address::from_str(CURVE_AAVE_POOL)?,
            self.curve_abi.clone(),
            self.client.clone(),
        );
        let idx = |s: &str| -> Option<i128> {
            match s {
                "DAI" => Some(0),
                "USDC" => Some(1),
                "USDT" => Some(2),
                _ => None,
            }
        };
        let mut out = HashMap::new();
        for &a in STABLE_SYMBOLS {
            for &b in STABLE_SYMBOLS {
                if a == b {
                    continue;
                }
                let (Some(i), Some(j)) = (idx(a), idx(b)) else {
                    continue;
                };
                let (_, dec_a) = self.resolve(a).await?;
                let (_, dec_b) = self.resolve(b).await?;
                let amount_in = quote_amount_for_usd(a, dec_a, self.notional_usd).await?;
                self.pace().await;
                let call = pool.method::<_, U256>("get_dy", (i, j, amount_in))?;
                match call.block(block).call().await {
                    Ok(dy) if !dy.is_zero() => {
                        let price = calculate_price_from_decimals(amount_in, dy, dec_a, dec_b)?;
                        if price > 0.0 {
                            out.insert((a.to_string(), b.to_string()), price);
                        }
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let s = e.to_string();
                        if paper_validation::is_archive_state_error(&s) {
                            return Err(anyhow!("archive state unavailable (curve get_dy): {s}"));
                        }
                    }
                }
            }
        }
        Ok(out)
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
    for &a in STABLE_SYMBOLS {
        for &b in STABLE_SYMBOLS {
            if a != b {
                set.insert((a.to_string(), b.to_string()));
            }
        }
    }
    set.into_iter().collect()
}

/// CPMM price_map (finder) + QuoteIndex (inclui Curve) para cross-model.
async fn build_quotes_at_block(
    quoter: &HistQuoter,
    metas: &[PairMeta],
    block_num: u64,
) -> Result<(HashMap<String, HashMap<String, f64>>, QuoteIndex)> {
    let mut out: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut index = QuoteIndex::new();

    let v3 = quoter.quote_v3_multicall(metas, block_num).await?;
    for ((a, b), (price, fee)) in &v3 {
        out.entry("UniswapV3".into())
            .or_default()
            .insert(format!("{a}-{b}"), *price);
        index.insert(
            ("UniswapV3".into(), a.clone(), b.clone()),
            (*price, Some(*fee)),
        );
    }

    let qs = Address::from_str(QUICKSWAP_ROUTER)?;
    let sushi = Address::from_str(SUSHISWAP_ROUTER)?;
    for (dex, router) in [("QuickSwap", qs), ("SushiSwap", sushi)] {
        let map = quoter
            .quote_v2_multicall(dex, router, metas, block_num)
            .await?;
        for ((a, b), price) in map {
            out.entry(dex.into())
                .or_default()
                .insert(format!("{a}-{b}"), price);
            index.insert((dex.into(), a, b), (price, None));
        }
    }

    let curve = quoter.quote_curve_stables(block_num).await?;
    for ((a, b), price) in curve {
        out.entry("Curve".into())
            .or_default()
            .insert(format!("{a}-{b}"), price);
        index.insert(("Curve".into(), a, b), (price, None));
    }
    Ok((out, index))
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
            "window,block,quote_block,eth_call_block,same_model_rate,cross_model_rate,cross_model,quote_only,best_cycle_rate,fee_floor,edge_exists,edge_same,edge_cross,pair,route,fee_tiers,models,net_previsto_usd,profit_realizado_usd,erro_rel_pct,sim_ok,revert_reason,archive_error"
        )?;
    }
    writeln!(
        f,
        "{},{},{},{},{:.10},{:.10},{},{},{:.10},{:.10},{},{},{},{},{},{},{},{},{},{},{},{},{}",
        sample.window_label.replace(',', ";"),
        sample.block,
        sample.quote_block,
        sample
            .eth_call_block
            .map(|b| b.to_string())
            .unwrap_or_default(),
        sample.same_model_rate,
        sample.cross_model_rate,
        sample.cross_model,
        sample.quote_only,
        sample.best_cycle_rate,
        sample.fee_floor,
        sample.edge_exists,
        sample.edge_same_model,
        sample.edge_cross_model,
        sample.pair,
        sample.route.replace(',', ";"),
        sample.fee_tiers,
        sample.models.replace(',', ";"),
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
// Scan one window
// ---------------------------------------------------------------------------

async fn scan_window(
    quoter: &HistQuoter,
    metas: &[PairMeta],
    cfg: &Config,
    discovery: &Config,
    engine: &ArbitrageEngine,
    arb_client: &ArbitrageClient,
    from: u64,
    to: u64,
    step: u64,
    label: &str,
    profile: &str,
    max_eth: u64,
    eth_calls: &mut u64,
    csv_path: &Path,
    first_csv: &mut bool,
) -> Result<(ReplayScanSummary, Vec<ReplayBlockSample>)> {
    let blocks = sample_block_list(from, to, step);
    if step <= 1 {
        let expect = expected_contiguous_count(from, to);
        if blocks.len() as u64 != expect {
            bail!(
                "contiguous undersample: got {} expected {} for [{from}..{to}] step={step}",
                blocks.len(),
                expect
            );
        }
    }

    info!(
        target: "replay_scan",
        label,
        profile,
        from,
        to,
        step,
        n_blocks = blocks.len(),
        range_size = expected_contiguous_count(from, to),
        "📼 REPLAY WINDOW start (dense contiguous={})",
        step <= 1
    );

    let t0 = Instant::now();
    let mut samples = Vec::with_capacity(blocks.len());

    for (i, &block) in blocks.iter().enumerate() {
        let mut sample = ReplayBlockSample {
            window_label: label.to_string(),
            block,
            quote_block: block,
            eth_call_block: None,
            same_model_rate: 0.0,
            cross_model_rate: 0.0,
            cross_model: false,
            quote_only: false,
            best_cycle_rate: 0.0,
            fee_floor: 0.0,
            edge_exists: false,
            edge_same_model: false,
            edge_cross_model: false,
            pair: String::new(),
            route: String::new(),
            fee_tiers: String::new(),
            models: String::new(),
            net_previsto_usd: None,
            profit_realizado_usd: None,
            erro_rel_pct: None,
            sim_ok: None,
            revert_reason: None,
            archive_error: None,
        };

        match build_quotes_at_block(quoter, metas, block).await {
            Ok((prices, quote_idx)) => {
                // SAME_MODEL: finder midcap CPMM (baseline)
                let opps = engine
                    .find_arbitrage_opportunities(&prices, discovery)
                    .await;
                let mut same_opp: Option<&ArbitrageOpportunity> = None;
                if let Some((rate, opp)) = best_tri_from_opps(&opps) {
                    sample.same_model_rate = rate;
                    sample.edge_same_model = edge_exists(rate);
                    same_opp = Some(opp);
                }

                // CROSS_MODEL: Curve × CPMM stables
                if let Some(cross) = find_best_stable_cycle(&quote_idx, true) {
                    sample.cross_model = true;
                    sample.cross_model_rate = cross.cycle_rate;
                    sample.edge_cross_model = edge_exists(cross.cycle_rate);
                    sample.quote_only = !replay_cross_model::route_all_legs_executable(
                        cross.venues().into_iter(),
                    );
                    // Prefer cross route in CSV when present (inspection)
                    sample.pair = cross.pair_label();
                    sample.route = cross.route_label();
                    sample.fee_tiers = cross.fee_tiers_label();
                    sample.models = cross
                        .legs
                        .iter()
                        .map(|l| l.model().as_str().to_string())
                        .collect::<Vec<_>>()
                        .join("+");
                    sample.fee_floor = cross.fee_floor;
                    sample.best_cycle_rate = cross.cycle_rate;
                } else if let Some(opp) = same_opp {
                    let (route, fees) = route_and_fees(opp);
                    sample.pair = opp.pair.clone();
                    sample.route = route;
                    sample.fee_tiers = fees;
                    sample.models = opp
                        .steps
                        .0
                        .iter()
                        .map(|s| replay_cross_model::venue_curve_model(&s.dex_name).as_str())
                        .collect::<Vec<_>>()
                        .join("+");
                    sample.fee_floor = theoretical_fee_floor(&opp.steps.0);
                    sample.best_cycle_rate = sample.same_model_rate;
                }

                sample.edge_exists = sample.edge_same_model || sample.edge_cross_model;

                // eth_call: only same-model executable edges (Curve = quote-only)
                if let Some(opp) = same_opp {
                    let executable = paper_validation::route_executor_supported(opp);
                    if should_eth_call_for_route(
                        sample.edge_same_model,
                        *eth_calls,
                        max_eth,
                        executable,
                    ) {
                        assert_same_block_tag(sample.quote_block, block)?;
                        let slip = cfg.flashloan.slippage_bps.unwrap_or(15) as u64;
                        let fl_fee = if cfg.flashloan.enabled {
                            opp.estimated_volume_usd * cfg.flashloan.fee_pct.unwrap_or(0.0005)
                        } else {
                            0.0
                        };
                        let would = paper_validation::would_execute(opp.spread_percent, cfg);
                        match arb_client
                            .paper_validate_at_block(opp, block, slip, fl_fee, would)
                            .await
                        {
                            Ok(ps) => {
                                *eth_calls += 1;
                                sample.eth_call_block = Some(ps.block_number);
                                let _ = assert_same_block_tag(sample.quote_block, ps.block_number);
                                sample.sim_ok = Some(ps.sim_ok);
                                sample.profit_realizado_usd = ps.profit_realizado_usd;
                                sample.erro_rel_pct = ps.erro_rel_pct;
                                sample.revert_reason = ps.revert_reason;
                                sample.net_previsto_usd = Some(ps.net_previsto_usd);
                                sample.quote_only = false;
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
                    } else if sample.edge_cross_model && sample.quote_only {
                        sample.revert_reason = Some(
                            "cross_model QUOTE_ONLY: Curve not in FlashloanExecutor DexType"
                                .into(),
                        );
                    }
                } else if sample.edge_cross_model {
                    sample.quote_only = true;
                    sample.revert_reason = Some(
                        "cross_model QUOTE_ONLY: Curve not in FlashloanExecutor DexType".into(),
                    );
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

        if let Err(e) = append_csv(csv_path, &sample, *first_csv) {
            warn!(target: "replay_scan", "csv: {e}");
        }
        *first_csv = false;
        samples.push(sample);

        let n_done = (i as u64) + 1;
        if n_done % THROUGHPUT_LOG_EVERY == 0 || n_done == blocks.len() as u64 {
            let elapsed = t0.elapsed().as_secs_f64().max(1e-9);
            info!(
                target: "replay_scan",
                label,
                done = n_done,
                total = blocks.len(),
                thrpt = format!("{:.2} blk/s", n_done as f64 / elapsed),
                "📼 REPLAY progress"
            );
        }
    }

    let elapsed = t0.elapsed();
    let summary =
        ReplayScanSummary::from_samples(&samples, from, to, step, label, profile, Some(elapsed));
    summary.log("WINDOW");
    Ok((summary, samples))
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
    let windows = resolve_windows(replay, latest, latest_ts)?;
    let step = replay.step.max(1);
    let max_eth = replay.max_eth_calls;

    info!(
        target: "replay_scan",
        n_windows = windows.len(),
        step,
        max_eth,
        curve_mode = if replay_cross_model::curve_executor_supported() {
            "executable"
        } else {
            "QUOTE_ONLY"
        },
        diag = replay_cross_model::curve_execution_diagnostic(),
        "📼 REPLAY SCAN start (paper-only, archive, multicall+curve get_dy)"
    );
    for (lo, hi, label, profile) in &windows {
        info!(
            target: "replay_scan",
            %label,
            %profile,
            from = lo,
            to = hi,
            n = expected_contiguous_count(*lo, *hi),
            "📼 REPLAY window planned"
        );
    }

    let mut quoter = HistQuoter::new(client.clone(), cfg.clone()).await?;
    quoter.assert_stable_decimals().await?;

    let (curve_tvl_usd, curve_passes_b2) = quoter.probe_curve_liquidity(&cfg, latest).await?;
    quoter.curve_liquid = curve_passes_b2;
    if curve_passes_b2 {
        info!(
            target: "replay_scan",
            pool = CURVE_AAVE_POOL,
            tvl_usd = curve_tvl_usd,
            min_usd = min_pool_liquidity_usd(&cfg),
            "Curve Aave pool (amDAI/amUSDC/amUSDT) @ block {} — B2 PASS",
            latest
        );
    } else {
        note_low_liquidity_discarded_pub(1);
        warn!(
            target: "replay_scan",
            pool = CURVE_AAVE_POOL,
            tvl_usd = curve_tvl_usd,
            min_usd = min_pool_liquidity_usd(&cfg),
            "Curve Aave pool @ block {} — B2 FAIL, cross_model desabilitado nesta run",
            latest
        );
    }
    let pairs = triangular_pairs(&cfg);
    let metas = quoter.prepare_pairs(&pairs).await?;
    let csv_path = Path::new(&replay.csv_path);
    let _ = std::fs::remove_file(csv_path);

    let mut discovery = (*cfg).clone();
    discovery.arbitrage.min_spread_percent = "-50.0".into();
    discovery.arbitrage.min_profit_threshold_usd = Some(-1.0e9);

    let mut eth_calls = 0u64;
    let mut first_csv = true;
    let mut window_summaries = Vec::new();
    let mut all_samples = Vec::new();

    for (from, to, label, profile) in windows {
        let (sum, samples) = scan_window(
            &quoter,
            &metas,
            &cfg,
            &discovery,
            engine,
            arb_client,
            from,
            to,
            step,
            &label,
            &profile,
            max_eth,
            &mut eth_calls,
            csv_path,
            &mut first_csv,
        )
        .await?;
        all_samples.extend(samples);
        window_summaries.push(sum);
    }

    let aggregate = if window_summaries.len() == 1 {
        window_summaries.pop().unwrap()
    } else {
        // Prefer aggregate from all samples for accurate percentiles/histogram
        let from = window_summaries
            .iter()
            .map(|s| s.block_from)
            .min()
            .unwrap_or(0);
        let to = window_summaries
            .iter()
            .map(|s| s.block_to)
            .max()
            .unwrap_or(0);
        let elapsed: Duration = window_summaries
            .iter()
            .filter_map(|s| s.elapsed_secs.map(Duration::from_secs_f64))
            .sum();
        let mut agg = ReplayScanSummary::from_samples(
            &all_samples,
            from,
            to,
            step,
            "AGGREGATE",
            "multi_window",
            Some(elapsed),
        );
        // Contiguous full only if every window was full AND step=1
        agg.contiguous_full = window_summaries.iter().all(|w| w.contiguous_full);
        agg.n_blocos_range = window_summaries.iter().map(|w| w.n_blocos_range).sum();
        for w in &window_summaries {
            w.log("WINDOW_FINAL");
        }
        agg
    };

    aggregate.log("AGGREGATE");
    Ok(aggregate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ReplayWindow};
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

    #[test]
    fn step1_samples_all_contiguous_blocks() {
        let from = 100u64;
        let to = 105u64;
        let blocks = sample_block_list(from, to, 1);
        assert_eq!(blocks.len() as u64, expected_contiguous_count(from, to));
        assert_eq!(blocks, vec![100, 101, 102, 103, 104, 105]);
        // step>1 undersamples — must NOT claim contiguous_full
        let sparse = sample_block_list(from, to, 2);
        assert!((sparse.len() as u64) < expected_contiguous_count(from, to));
    }

    #[test]
    fn eth_call_only_on_edge_and_under_cap() {
        assert!(!should_eth_call(false, 0, 40));
        assert!(should_eth_call(true, 0, 40));
        assert!(should_eth_call(true, 39, 40));
        assert!(!should_eth_call(true, 40, 40));
        assert!(!should_eth_call(true, 100, 40));
    }

    #[test]
    fn windows_priority_over_legacy_range() {
        let mut cfg = ReplayConfig::default();
        cfg.block_from = Some(1);
        cfg.block_to = Some(10);
        cfg.windows = vec![
            ReplayWindow {
                from: 50,
                to: 52,
                label: "a".into(),
                profile: "high_vol".into(),
            },
            ReplayWindow {
                from: 90,
                to: 91,
                label: "b".into(),
                profile: "control".into(),
            },
        ];
        let wins = resolve_windows(&cfg, 1000, 1_700_000_000).unwrap();
        assert_eq!(wins.len(), 2);
        assert_eq!(wins[0].0, 50);
        assert_eq!(wins[0].1, 52);
        assert_eq!(wins[0].2, "a");
        assert_eq!(wins[1].0, 90);
    }

    #[test]
    fn multi_window_aggregate_sums_counts() {
        let s1 = vec![
            ReplayBlockSample {
                window_label: "w1".into(),
                block: 1,
                quote_block: 1,
                eth_call_block: Some(1),
                same_model_rate: 1.001,
                cross_model_rate: 0.0,
                cross_model: false,
                quote_only: false,
                best_cycle_rate: 1.001,
                fee_floor: 0.99,
                edge_exists: true,
                edge_same_model: true,
                edge_cross_model: false,
                pair: "A".into(),
                route: String::new(),
                fee_tiers: String::new(),
                models: "Cpmm+Cpmm+Cpmm".into(),
                net_previsto_usd: Some(1.0),
                profit_realizado_usd: Some(0.5),
                erro_rel_pct: Some(10.0),
                sim_ok: Some(true),
                revert_reason: None,
                archive_error: None,
            },
            ReplayBlockSample {
                window_label: "w1".into(),
                block: 2,
                quote_block: 2,
                eth_call_block: None,
                same_model_rate: 0.996,
                cross_model_rate: 1.002,
                cross_model: true,
                quote_only: true,
                best_cycle_rate: 1.002,
                fee_floor: 0.99,
                edge_exists: true,
                edge_same_model: false,
                edge_cross_model: true,
                pair: "B".into(),
                route: "Curve[StableSwap]:USDC→USDT|UniswapV3[Cpmm]:USDT→DAI|QuickSwap[Cpmm]:DAI→USDC".into(),
                fee_tiers: "4bps;100;-".into(),
                models: "StableSwap+Cpmm+Cpmm".into(),
                net_previsto_usd: None,
                profit_realizado_usd: None,
                erro_rel_pct: None,
                sim_ok: None,
                revert_reason: Some("cross_model QUOTE_ONLY".into()),
                archive_error: None,
            },
        ];
        let s2 = vec![ReplayBlockSample {
            window_label: "w2".into(),
            block: 10,
            quote_block: 10,
            eth_call_block: None,
            same_model_rate: 0.994,
            cross_model_rate: 0.995,
            cross_model: true,
            quote_only: true,
            best_cycle_rate: 0.995,
            fee_floor: 0.99,
            edge_exists: false,
            edge_same_model: false,
            edge_cross_model: false,
            pair: "C".into(),
            route: String::new(),
            fee_tiers: String::new(),
            models: String::new(),
            net_previsto_usd: None,
            profit_realizado_usd: None,
            erro_rel_pct: None,
            sim_ok: None,
            revert_reason: None,
            archive_error: None,
        }];
        let w1 = ReplayScanSummary::from_samples(&s1, 1, 2, 1, "w1", "high_vol", None);
        let w2 = ReplayScanSummary::from_samples(&s2, 10, 10, 1, "w2", "control", None);
        assert!(w1.contiguous_full);
        assert!(w2.contiguous_full);
        assert_eq!(w1.same_model.n_blocos_com_edge, 1);
        assert_eq!(w1.cross_model.n_blocos_com_edge, 1);
        assert_eq!(w1.cross_model.n_quote_only_edges, 1);
        assert_eq!(w1.same_model.n_sim_ok, 1);
        assert_eq!(w1.cross_model.histogram.gte_1_0, 1);

        let mut all = s1;
        all.extend(s2);
        let agg = ReplayScanSummary::from_samples(&all, 1, 10, 1, "AGGREGATE", "multi_window", None);
        assert_eq!(agg.n_blocos_amostrados, 3);
        assert_eq!(agg.same_model.n_blocos_com_edge, 1);
        assert_eq!(agg.cross_model.n_blocos_com_edge, 1);
        assert_eq!(agg.curve_mode, "QUOTE_ONLY");
    }

    #[test]
    fn committed_windows_step1_contiguous() {
        // Same ranges as config.toml — must sample every block.
        let windows = [
            (90772627u64, 90773526u64),
            (90776227, 90777126),
            (90815827, 90816726),
        ];
        for (from, to) in windows {
            let blocks = sample_block_list(from, to, 1);
            assert_eq!(
                blocks.len() as u64,
                expected_contiguous_count(from, to),
                "undersample [{from}..{to}]"
            );
        }
    }
}
