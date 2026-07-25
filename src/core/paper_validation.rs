//! Paper validation gate — compara net previsto do engine vs profit real via eth_call.
//!
//! **SEGURANÇA:** quando ativo (`PAPER_VALIDATION=1` ou `[validation].paper_enabled` /
//! `dry_run_only`), nenhum `send()`/broadcast é permitido. Early-return em
//! `ArbitrageClient::send_and_confirm_transaction` e approve.
//!
//! Profit realizado = delta de saldo ERC20 (before/after) no mesmo `blockTag`,
//! preferencialmente via uma única medição parseável; eth_call **não** persiste
//! estado entre RPCs separados, então o delta vem de:
//! 1) parser `(balance_before, balance_after)` (fixture / Multicall agregado), ou
//! 2) resposta `alchemy_simulateAssetChanges` quando o RPC suportar.
//!
//! CSV append ocorre em task async (channel) — **fora** do hot path do radar.

use crate::{
    config::Config,
    contracts::ERC20,
    core::types::ArbitrageOpportunity,
    AppMiddleware,
};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use ethers::{
    abi::{decode, encode, ParamType, Token},
    prelude::*,
    types::{Address, BlockId, BlockNumber, Bytes, H256, U256},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs::OpenOptions,
    io::Write,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex as StdMutex,
    },
};
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Env que força paper mode (envio fisicamente impossível).
pub const ENV_PAPER_VALIDATION: &str = "PAPER_VALIDATION";

/// Env opcional: endereço `from` para eth_call (nunca keypair).
pub const ENV_PAPER_FROM: &str = "PAPER_FROM";

// ---------------------------------------------------------------------------
// Config helpers
// ---------------------------------------------------------------------------

/// True se envio de TX deve ser barrado (paper / dry_run_only / env).
pub fn sends_forbidden(cfg: &Config) -> bool {
    paper_mode_active(cfg) || cfg.validation.dry_run_only || cfg.execution.dry_run
}

/// Paper validation ligado (env ou config).
pub fn paper_mode_active(cfg: &Config) -> bool {
    env_paper_flag() || cfg.validation.paper_enabled
}

/// `PAPER_VALIDATION=1|true|yes|on`
pub fn env_paper_flag() -> bool {
    env_flag_true(ENV_PAPER_VALIDATION)
}

fn env_flag_true(name: &str) -> bool {
    std::env::var(name)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Balance delta math (testável sem RPC)
// ---------------------------------------------------------------------------

/// Delta assinado `after - before` em unidades raw do token.
pub fn balance_delta_raw(before: U256, after: U256) -> i128 {
    if after >= before {
        let d = after - before;
        d.as_u128() as i128
    } else {
        let d = before - after;
        -(d.as_u128() as i128)
    }
}

/// Converte delta raw → USD.
pub fn balance_delta_usd(delta_raw: i128, decimals: u32, token_price_usd: f64) -> f64 {
    let denom = 10f64.powi(decimals as i32);
    (delta_raw as f64 / denom) * token_price_usd
}

/// Decodifica retorno `abi.encode(uint256 before, uint256 after)` → delta USD.
pub fn parse_balance_pair_abi(
    data: &[u8],
    decimals: u32,
    token_price_usd: f64,
) -> Result<(U256, U256, f64)> {
    let tokens = decode(
        &[ParamType::Uint(256), ParamType::Uint(256)],
        data,
    )
    .context("decode balance pair ABI")?;
    let before = match &tokens[0] {
        Token::Uint(v) => *v,
        _ => return Err(anyhow!("expected uint before")),
    };
    let after = match &tokens[1] {
        Token::Uint(v) => *v,
        _ => return Err(anyhow!("expected uint after")),
    };
    let usd = balance_delta_usd(balance_delta_raw(before, after), decimals, token_price_usd);
    Ok((before, after, usd))
}

/// Encode fixture `(before, after)` — espelha o que um Multicall agregado devolveria.
pub fn encode_balance_pair_abi(before: U256, after: U256) -> Bytes {
    Bytes::from(encode(&[Token::Uint(before), Token::Uint(after)]))
}

/// Extrai delta do token `asset` para `holder` a partir de JSON
/// `alchemy_simulateAssetChanges` (ou fixture equivalente).
pub fn parse_alchemy_asset_changes_delta(
    json: &Value,
    asset: Address,
    holder: Address,
) -> Option<i128> {
    let changes = json
        .get("changes")
        .or_else(|| json.pointer("/result/changes"))
        .and_then(|c| c.as_array())?;

    let asset_l = format!("{:?}", asset).to_ascii_lowercase();
    let holder_l = format!("{:?}", holder).to_ascii_lowercase();
    let mut delta: i128 = 0;

    for ch in changes {
        let contract = ch
            .get("contractAddress")
            .or_else(|| ch.get("contract_address"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if !contract.is_empty() && contract != asset_l {
            continue;
        }

        let raw = ch
            .get("rawAmount")
            .or_else(|| ch.get("raw_amount"))
            .or_else(|| ch.get("change"))
            .and_then(|v| v.as_str())
            .unwrap_or("0");

        let amount = parse_i128_amount(raw).unwrap_or(0);
        let to = ch
            .get("to")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let from = ch
            .get("from")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        if to == holder_l {
            delta += amount.abs();
        }
        if from == holder_l {
            delta -= amount.abs();
        }
        // Alguns payloads já trazem sinal em `change`
        if ch.get("to").is_none() && ch.get("from").is_none() {
            delta += amount;
        }
    }

    Some(delta)
}

fn parse_i128_amount(s: &str) -> Option<i128> {
    let t = s.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        // hex unsigned magnitude; sign handled by from/to
        u128::from_str_radix(hex, 16).ok().map(|v| v as i128)
    } else {
        t.parse::<i128>().ok()
    }
}

// ---------------------------------------------------------------------------
// Sample + aggregate
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaperSample {
    pub timestamp: String,
    pub pair: String,
    pub route: String,
    pub fee_tiers: String,
    pub trade_usd: f64,
    pub net_previsto_usd: f64,
    pub gross_previsto_usd: f64,
    pub gas_usd: f64,
    pub flashloan_fee_usd: f64,
    pub profit_realizado_usd: Option<f64>,
    pub erro_abs_usd: Option<f64>,
    pub erro_rel_pct: Option<f64>,
    pub block_number: u64,
    pub sim_ok: bool,
    pub revert_reason: Option<String>,
    pub false_profitable: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PaperAggregate {
    pub n_amostras: u64,
    pub n_reverts: u64,
    pub n_falsos_lucrativos: u64,
    pub erro_rel_pct_mean: Option<f64>,
    pub erro_rel_pct_p50: Option<f64>,
    pub erro_rel_pct_p95: Option<f64>,
}

impl PaperAggregate {
    pub fn from_samples(samples: &[PaperSample]) -> Self {
        let n = samples.len() as u64;
        let n_reverts = samples.iter().filter(|s| !s.sim_ok).count() as u64;
        let n_falsos = samples.iter().filter(|s| s.false_profitable).count() as u64;

        let mut errs: Vec<f64> = samples.iter().filter_map(|s| s.erro_rel_pct).collect();
        errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let mean = if errs.is_empty() {
            None
        } else {
            Some(errs.iter().sum::<f64>() / errs.len() as f64)
        };

        Self {
            n_amostras: n,
            n_reverts,
            n_falsos_lucrativos: n_falsos,
            erro_rel_pct_mean: mean,
            erro_rel_pct_p50: percentile(&errs, 50.0),
            erro_rel_pct_p95: percentile(&errs, 95.0),
        }
    }

    pub fn log_summary(&self) {
        info!(
            target: "paper_validation",
            "📊 PAPER SUMMARY | n={} reverts={} falsos_lucrativos={} erro_rel% mean={:?} p50={:?} p95={:?}",
            self.n_amostras,
            self.n_reverts,
            self.n_falsos_lucrativos,
            self.erro_rel_pct_mean,
            self.erro_rel_pct_p50,
            self.erro_rel_pct_p95
        );
    }
}

fn percentile(sorted: &[f64], p: f64) -> Option<f64> {
    if sorted.is_empty() {
        return None;
    }
    // nearest-rank with floor so p50 of [20,110] = 20 (lower median for even n-1 path)
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).floor() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

pub fn build_sample(
    opp: &ArbitrageOpportunity,
    block_number: u64,
    flashloan_fee_usd: f64,
    profit_realizado_usd: Option<f64>,
    sim_ok: bool,
    revert_reason: Option<String>,
) -> PaperSample {
    let fee_tiers: Vec<String> = opp
        .steps
        .0
        .iter()
        .map(|s| {
            s.v3_fee_tier
                .map(|f| f.to_string())
                .unwrap_or_else(|| "-".into())
        })
        .collect();

    let route = opp
        .steps
        .0
        .iter()
        .map(|s| format!("{}:{}→{}", s.dex_name, s.token_in, s.token_out))
        .collect::<Vec<_>>()
        .join("|");

    let (erro_abs, erro_rel, false_profitable) = match profit_realizado_usd {
        Some(real) => {
            let abs = (opp.net_profit_usd - real).abs();
            let rel = if opp.net_profit_usd.abs() > 1e-12 {
                Some((abs / opp.net_profit_usd.abs()) * 100.0)
            } else if real.abs() > 1e-12 {
                Some(100.0)
            } else {
                Some(0.0)
            };
            let fp = opp.net_profit_usd > 0.0 && real <= 0.0;
            (Some(abs), rel, fp)
        }
        None => (None, None, !sim_ok && opp.net_profit_usd > 0.0),
    };

    PaperSample {
        timestamp: Utc::now().format("%Y-%m-%d %H:%M:%S").to_string(),
        pair: opp.pair.clone(),
        route,
        fee_tiers: fee_tiers.join(";"),
        trade_usd: opp.estimated_volume_usd,
        net_previsto_usd: opp.net_profit_usd,
        gross_previsto_usd: opp.estimated_profit_usd,
        gas_usd: opp.gas_cost_usd,
        flashloan_fee_usd,
        profit_realizado_usd,
        erro_abs_usd: erro_abs,
        erro_rel_pct: erro_rel,
        block_number,
        sim_ok,
        revert_reason,
        false_profitable,
    }
}

// ---------------------------------------------------------------------------
// Async CSV writer (fora do hot path do radar)
// ---------------------------------------------------------------------------

pub struct PaperValidationHub {
    tx: mpsc::Sender<PaperSample>,
    aggregate: Arc<StdMutex<Vec<PaperSample>>>,
    summary_window: u64,
    sample_count: Arc<AtomicU64>,
}

impl PaperValidationHub {
    /// Spawna writer async. Append CSV **nesta task**, nunca no loop de bloco.
    pub fn spawn(csv_path: PathBuf, summary_window: u64) -> Arc<Self> {
        let (tx, mut rx) = mpsc::channel::<PaperSample>(256);
        let aggregate = Arc::new(StdMutex::new(Vec::new()));
        let agg2 = aggregate.clone();
        let sample_count = Arc::new(AtomicU64::new(0));
        let count2 = sample_count.clone();

        tokio::spawn(async move {
            if let Some(parent) = csv_path.parent() {
                let _ = tokio::fs::create_dir_all(parent).await;
            }
            let path = csv_path;
            while let Some(sample) = rx.recv().await {
                let sample_clone = sample.clone();
                let path_c = path.clone();
                let write_res = tokio::task::spawn_blocking(move || {
                    let need_header = !path_c.exists();
                    append_csv_row(&path_c, &sample_clone, need_header)
                })
                .await;

                if let Ok(Err(e)) = write_res {
                    warn!(target: "paper_validation", "CSV write failed: {e}");
                }

                let n = {
                    let mut guard = agg2.lock().unwrap();
                    guard.push(sample);
                    let n = guard.len();
                    if summary_window > 0 && n as u64 % summary_window == 0 {
                        let agg = PaperAggregate::from_samples(&guard);
                        agg.log_summary();
                    }
                    n
                };
                count2.store(n as u64, Ordering::Relaxed);
            }

            // flush summary on channel close
            let guard = agg2.lock().unwrap();
            if !guard.is_empty() {
                PaperAggregate::from_samples(&guard).log_summary();
            }
        });

        Arc::new(Self {
            tx,
            aggregate,
            summary_window,
            sample_count,
        })
    }

    pub fn try_submit(&self, sample: PaperSample) {
        match self.tx.try_send(sample) {
            Ok(()) => {}
            Err(mpsc::error::TrySendError::Full(s)) => {
                warn!(target: "paper_validation", "paper CSV channel full — dropping sample {}", s.pair);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                warn!(target: "paper_validation", "paper CSV channel closed");
            }
        }
    }

    pub fn snapshot_aggregate(&self) -> PaperAggregate {
        let guard = self.aggregate.lock().unwrap();
        PaperAggregate::from_samples(&guard)
    }

    pub fn len(&self) -> u64 {
        self.sample_count.load(Ordering::Relaxed)
    }

    pub fn summary_window(&self) -> u64 {
        self.summary_window
    }
}

fn append_csv_row(path: &PathBuf, s: &PaperSample, write_header: bool) -> std::io::Result<()> {
    let exists = path.exists();
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if write_header && !exists {
        writeln!(
            f,
            "timestamp,pair,route,fee_tiers,trade_usd,net_previsto_usd,gross_previsto_usd,gas_usd,flashloan_fee_usd,profit_realizado_usd,erro_abs_usd,erro_rel_pct,block_number,sim_ok,revert_reason,false_profitable"
        )?;
    }
    writeln!(
        f,
        "{},{},{},{},{:.8},{:.8},{:.8},{:.8},{:.8},{},{},{},{},{},{},{}",
        s.timestamp,
        s.pair,
        escape_csv(&s.route),
        s.fee_tiers,
        s.trade_usd,
        s.net_previsto_usd,
        s.gross_previsto_usd,
        s.gas_usd,
        s.flashloan_fee_usd,
        opt_f64(s.profit_realizado_usd),
        opt_f64(s.erro_abs_usd),
        opt_f64(s.erro_rel_pct),
        s.block_number,
        s.sim_ok,
        s.revert_reason.as_deref().unwrap_or(""),
        s.false_profitable,
    )?;
    Ok(())
}

fn opt_f64(v: Option<f64>) -> String {
    v.map(|x| format!("{:.8}", x)).unwrap_or_default()
}

fn escape_csv(s: &str) -> String {
    if s.contains(',') || s.contains('"') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// On-chain helpers
// ---------------------------------------------------------------------------

pub async fn erc20_balance(
    client: Arc<AppMiddleware>,
    token: Address,
    holder: Address,
    block: BlockId,
) -> Result<U256> {
    let erc20 = ERC20::new(token, client);
    let bal: U256 = erc20.balance_of(holder).block(block).call().await?;
    Ok(bal)
}

/// Tenta `alchemy_simulateAssetChanges`; retorna delta raw do asset para holder.
pub async fn try_alchemy_asset_delta(
    client: &AppMiddleware,
    from: Address,
    to: Address,
    data: Bytes,
    asset: Address,
    holder: Address,
) -> Option<i128> {
    #[derive(Debug, Serialize)]
    struct TxParam {
        from: String,
        to: String,
        data: String,
    }
    let tx = TxParam {
        from: format!("{:?}", from),
        to: format!("{:?}", to),
        data: format!("{}", data),
    };
    let provider = client.provider();
    let raw: Result<Value, _> = provider
        .request("alchemy_simulateAssetChanges", [tx])
        .await;
    match raw {
        Ok(v) => parse_alchemy_asset_changes_delta(&v, asset, holder),
        Err(e) => {
            debug_alchemy_fail(&e);
            None
        }
    }
}

fn debug_alchemy_fail(e: &impl std::fmt::Display) {
    tracing::debug!(target: "paper_validation", "alchemy_simulateAssetChanges unavailable: {e}");
}

pub fn resolve_paper_from(cfg: &Config, wallet: Address) -> Address {
    if let Ok(s) = std::env::var(ENV_PAPER_FROM) {
        if let Ok(addr) = s.trim().parse::<Address>() {
            return addr;
        }
    }
    if !cfg.validation.paper_from.is_empty() {
        if let Ok(addr) = cfg.validation.paper_from.parse::<Address>() {
            return addr;
        }
    }
    wallet
}

pub async fn current_block_number(client: &AppMiddleware) -> Result<u64> {
    Ok(client.get_block_number().await?.as_u64())
}

pub fn block_id(n: u64) -> BlockId {
    BlockId::Number(BlockNumber::Number(n.into()))
}

/// Log estruturado de uma amostra paper.
pub fn log_sample(s: &PaperSample) {
    info!(
        target: "paper_validation",
        pair = %s.pair,
        block = s.block_number,
        net_previsto = s.net_previsto_usd,
        profit_real = ?s.profit_realizado_usd,
        erro_rel_pct = ?s.erro_rel_pct,
        sim_ok = s.sim_ok,
        false_profitable = s.false_profitable,
        revert = ?s.revert_reason,
        "PAPER sample"
    );
}

// silence unused H256 import in some builds
#[allow(dead_code)]
fn _keep_h256(_: H256) {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn balance_delta_parser_positive() {
        let before = U256::from(1_000_000u64);
        let after = U256::from(1_000_500u64);
        let d = balance_delta_raw(before, after);
        assert_eq!(d, 500);
        let usd = balance_delta_usd(d, 6, 1.0);
        assert!((usd - 0.0005).abs() < 1e-12);
    }

    #[test]
    fn balance_delta_abi_roundtrip() {
        let before = U256::from(100u64);
        let after = U256::from(250u64);
        let encoded = encode_balance_pair_abi(before, after);
        let (b, a, usd) = parse_balance_pair_abi(&encoded, 0, 1.0).unwrap();
        assert_eq!(b, before);
        assert_eq!(a, after);
        assert!((usd - 150.0).abs() < 1e-9);
    }

    #[test]
    fn alchemy_fixture_extracts_holder_delta() {
        let asset: Address = "0x2791bca1f2de4661ed88a30c99a7a9449aa84174"
            .parse()
            .unwrap();
        let holder: Address = "0x1111111111111111111111111111111111111111"
            .parse()
            .unwrap();
        let fixture = json!({
            "changes": [{
                "contractAddress": "0x2791bca1f2de4661ed88a30c99a7a9449aa84174",
                "rawAmount": "0x0f4240",
                "from": "0x0000000000000000000000000000000000000000",
                "to": "0x1111111111111111111111111111111111111111"
            }]
        });
        let d = parse_alchemy_asset_changes_delta(&fixture, asset, holder).unwrap();
        assert_eq!(d, 1_000_000); // 0xf4240
    }

    #[test]
    fn sends_forbidden_when_env_paper() {
        std::env::set_var(ENV_PAPER_VALIDATION, "1");
        let cfg = Config::default();
        assert!(paper_mode_active(&cfg));
        assert!(sends_forbidden(&cfg));
        std::env::remove_var(ENV_PAPER_VALIDATION);
    }

    #[test]
    fn sends_forbidden_when_dry_run_only() {
        std::env::remove_var(ENV_PAPER_VALIDATION);
        let mut cfg = Config::default();
        cfg.validation.dry_run_only = true;
        assert!(sends_forbidden(&cfg));
    }

    #[test]
    fn aggregate_counts_false_profitable() {
        let samples = vec![
            PaperSample {
                timestamp: "t".into(),
                pair: "A-B".into(),
                route: "r".into(),
                fee_tiers: "500".into(),
                trade_usd: 100.0,
                net_previsto_usd: 1.0,
                gross_previsto_usd: 2.0,
                gas_usd: 0.1,
                flashloan_fee_usd: 0.05,
                profit_realizado_usd: Some(-0.1),
                erro_abs_usd: Some(1.1),
                erro_rel_pct: Some(110.0),
                block_number: 1,
                sim_ok: true,
                revert_reason: None,
                false_profitable: true,
            },
            PaperSample {
                timestamp: "t".into(),
                pair: "A-B".into(),
                route: "r".into(),
                fee_tiers: "-".into(),
                trade_usd: 100.0,
                net_previsto_usd: 0.5,
                gross_previsto_usd: 1.0,
                gas_usd: 0.1,
                flashloan_fee_usd: 0.05,
                profit_realizado_usd: Some(0.4),
                erro_abs_usd: Some(0.1),
                erro_rel_pct: Some(20.0),
                block_number: 2,
                sim_ok: true,
                revert_reason: None,
                false_profitable: false,
            },
        ];
        let agg = PaperAggregate::from_samples(&samples);
        assert_eq!(agg.n_amostras, 2);
        assert_eq!(agg.n_falsos_lucrativos, 1);
        assert_eq!(agg.erro_rel_pct_p50, Some(20.0));
    }

    #[test]
    fn build_sample_marks_false_profitable() {
        let mut opp = ArbitrageOpportunity::default();
        opp.pair = "USDT-WMATIC".into();
        opp.net_profit_usd = 0.5;
        opp.estimated_profit_usd = 1.0;
        opp.estimated_volume_usd = 100.0;
        let s = build_sample(&opp, 99, 0.05, Some(0.0), true, None);
        assert!(s.false_profitable);
        assert!(s.erro_abs_usd.unwrap() > 0.0);
    }

    #[test]
    fn sends_forbidden_blocks_broadcast_path() {
        // Espelha o early-return de send_and_confirm_transaction.
        std::env::set_var(ENV_PAPER_VALIDATION, "true");
        let cfg = Config::default();
        assert!(sends_forbidden(&cfg));
        // Qualquer call site deve short-circuit ANTES de call.send()
        let mode = if sends_forbidden(&cfg) {
            "paper_send_blocked"
        } else {
            "would_send"
        };
        assert_eq!(mode, "paper_send_blocked");
        std::env::remove_var(ENV_PAPER_VALIDATION);
    }
}
