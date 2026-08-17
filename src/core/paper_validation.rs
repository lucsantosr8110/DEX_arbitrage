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
    config::Config, contracts::ERC20, core::economics, core::types::ArbitrageOpportunity,
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

/// Aave V3 Pool (Polygon) — mesmo endereço do FlashloanExecutor.sol.
pub const AAVE_V3_POOL_POLYGON: &str = "0x794a61358D6845594F94dc1DB02A252b5b4814aD";

// ---------------------------------------------------------------------------
// Revert decoding (Error(string) / Panic / custom)
// ---------------------------------------------------------------------------

/// Extrai e decodifica revert data de mensagens de provider / ethers.
pub fn decode_revert_message(err: &str) -> String {
    if let Some(hex_data) = extract_revert_hex(err) {
        let decoded = decode_revert_data_hex(&hex_data);
        // Preferir string ABI quando disponível
        if decoded.starts_with("Error(\"") {
            return decoded;
        }
        // Se só CustomError, ainda tenta mensagem plaintext do provider
        if let Some(plain) = extract_plain_revert_reason(err) {
            return format!("{decoded} | {plain}");
        }
        return decoded;
    }
    if let Some(plain) = extract_plain_revert_reason(err) {
        return plain;
    }
    err.to_string()
}

/// "execution reverted: Not profitable" / "message: execution reverted: Invalid initiator"
fn extract_plain_revert_reason(err: &str) -> Option<String> {
    for marker in ["execution reverted:", "execution reverted"] {
        if let Some(rest) = err.split(marker).nth(1) {
            let t = rest
                .trim()
                .trim_start_matches(':')
                .trim()
                .split(',')
                .next()
                .unwrap_or("")
                .trim()
                .trim_matches('"');
            if !t.is_empty() && !t.starts_with("data") && !t.starts_with("0x") && t.len() < 120 {
                return Some(t.to_string());
            }
        }
    }
    None
}

/// Hex do revert (com ou sem `0x`). Aceita vários formatos de erro RPC.
pub fn extract_revert_hex(err: &str) -> Option<String> {
    // ethers: data: Some(String("0x08c379a0...."))
    for marker in [
        "String(\"0x",
        "String(\"0X",
        "data: Some(String(\"0x",
        "\"data\":\"0x",
        "\"data\": \"0x",
        "data\":\"0x",
        "data: 0x",
        "data:0x",
    ] {
        if let Some(idx) = err.find(marker) {
            let start = idx + marker.len();
            // marker may already consume "0x"
            let rest = &err[start..];
            let hex: String = rest.chars().take_while(|c| c.is_ascii_hexdigit()).collect();
            if hex.len() >= 8 {
                return Some(hex);
            }
        }
    }
    None
}

/// Decodifica payload de revert ABI (sem prefixo 0x).
pub fn decode_revert_data_hex(hex_data: &str) -> String {
    let hex_data = hex_data
        .trim()
        .trim_start_matches("0x")
        .trim_start_matches("0X");
    if hex_data.len() < 8 {
        return format!("RevertData(short): 0x{hex_data}");
    }
    let selector = &hex_data[..8].to_ascii_lowercase();
    let body = hex::decode(&hex_data[8..]).unwrap_or_default();

    match selector.as_str() {
        "08c379a0" => {
            // Error(string)
            match decode(&[ParamType::String], &body) {
                Ok(tokens) => match &tokens[0] {
                    Token::String(s) => format!("Error(\"{s}\")"),
                    _ => "Error(string)".into(),
                },
                Err(_) => "Error(string) [decode failed]".into(),
            }
        }
        "4e487b71" => {
            // Panic(uint256)
            match decode(&[ParamType::Uint(256)], &body) {
                Ok(tokens) => match &tokens[0] {
                    Token::Uint(code) => {
                        let c = code.as_u32();
                        let name = match c {
                            0x01 => "assert",
                            0x11 => "arithmetic overflow",
                            0x12 => "division by zero",
                            0x21 => "enum conversion",
                            0x22 => "storage encoding",
                            0x31 => "empty array pop",
                            0x32 => "out of bounds",
                            0x41 => "out of memory",
                            0x51 => "uninitialized function",
                            _ => "unknown",
                        };
                        format!("Panic({c:#x}/{name})")
                    }
                    _ => "Panic(uint256)".into(),
                },
                Err(_) => "Panic(uint256) [decode failed]".into(),
            }
        }
        _ => {
            // Custom error — tenta decodificar 1º arg string se parecer ABI
            format!("CustomError(0x{selector})")
        }
    }
}

/// Motivo legível quando `executeFlashloan` retorna `false` (try/catch engole o revert).
pub const GENERIC_FLASHLOAN_FALSE: &str =
    "executeFlashloan returned false (Aave try/catch swallowed inner revert)";

// ---------------------------------------------------------------------------
// eth_call helpers (paper only — nunca send)
// ---------------------------------------------------------------------------

/// Monta calldata `flashLoanSimple(receiver, asset, amount, params, referralCode)`.
pub fn encode_aave_flash_loan_simple(
    receiver: Address,
    asset: Address,
    amount: U256,
    params: Bytes,
) -> Bytes {
    let selector = &ethers::utils::id("flashLoanSimple(address,address,uint256,bytes,uint16)")[..4];
    let encoded = encode(&[
        Token::Address(receiver),
        Token::Address(asset),
        Token::Uint(amount),
        Token::Bytes(params.to_vec()),
        Token::Uint(U256::from(0u64)), // uint16 referralCode
    ]);
    let mut out = Vec::with_capacity(4 + encoded.len());
    out.extend_from_slice(selector);
    out.extend_from_slice(&encoded);
    Bytes::from(out)
}

/// Params do executor: `abi.encode(initiator, steps)` — igual ao Solidity.
pub fn encode_executor_flashloan_params(initiator: Address, steps_abi_tokens: Token) -> Bytes {
    Bytes::from(encode(&[Token::Address(initiator), steps_abi_tokens]))
}

/// eth_call raw. `state_override` é **só simulação** (nunca altera chain).
/// Retorna Ok(retorno) ou Err(motivo decodificado).
pub async fn eth_call_raw(
    client: &AppMiddleware,
    from: Address,
    to: Address,
    data: Bytes,
    block: BlockId,
    state_override: Option<Value>,
) -> std::result::Result<Bytes, String> {
    let tx = serde_json::json!({
        "from": format!("{:?}", from),
        "to": format!("{:?}", to),
        "data": format!("{}", data),
    });
    let block_tag = match block {
        BlockId::Number(BlockNumber::Number(n)) => {
            serde_json::json!(format!("0x{:x}", n.as_u64()))
        }
        BlockId::Number(BlockNumber::Latest) => serde_json::json!("latest"),
        _ => serde_json::json!("latest"),
    };

    let provider = client.provider();
    let raw: Result<Bytes, _> = if let Some(ovr) = state_override {
        provider.request("eth_call", (tx, block_tag, ovr)).await
    } else {
        provider.request("eth_call", (tx, block_tag)).await
    };

    match raw {
        Ok(b) => Ok(b),
        Err(e) => Err(decode_revert_message(&e.to_string())),
    }
}

/// Probe: chama Aave.flashLoanSimple com `from = executor` (mesmo msg.sender do
/// fluxo real em `executeFlashloan`). Assim `initiator == address(this)` e o
/// require "Invalid initiator" não mascara o revert econômico.
///
/// NÃO usar from=EOA aqui — Aave marcaria initiator=EOA → "Invalid initiator"
/// falso (o fluxo real chama o pool a partir do contrato executor).
pub async fn probe_aave_flashloan_revert(
    client: &AppMiddleware,
    _paper_from: Address,
    executor: Address,
    asset: Address,
    amount: U256,
    params: Bytes,
    block: BlockId,
    state_override: Option<Value>,
) -> String {
    let aave: Address = AAVE_V3_POOL_POLYGON.parse().expect("Aave pool addr");
    let data = encode_aave_flash_loan_simple(executor, asset, amount, params);
    // from = executor (endereço do contrato — eth_call não assina).
    match eth_call_raw(client, executor, aave, data, block, state_override).await {
        Ok(_) => {
            "Aave.flashLoanSimple succeeded on probe (unexpected vs executeFlashloan=false)".into()
        }
        Err(reason) => format!("Aave.flashLoanSimple revert: {reason}"),
    }
}

/// State override mínimo: saldo ERC20 de `holder` no slot de balance (mapping slot 0
/// padrão OZ — **pode falhar** em tokens com layout diferente). Só paper/simulação.
pub fn erc20_balance_state_override(token: Address, holder: Address, raw_balance: U256) -> Value {
    // mapping(address => uint256) balances at slot 0 (OpenZeppelin ERC20)
    let mut slot_key = [0u8; 64];
    slot_key[12..32].copy_from_slice(holder.as_bytes());
    // slot index 0 already zero in second half
    let slot = ethers::utils::keccak256(slot_key);
    let mut val = [0u8; 32];
    raw_balance.to_big_endian(&mut val);
    serde_json::json!({
        format!("{:?}", token): {
            "stateDiff": {
                format!("0x{}", hex::encode(slot)): format!("0x{}", hex::encode(val))
            }
        }
    })
}

/// True se overrides de estado estão habilitados (só fazem sentido em paper).
pub fn paper_state_overrides_enabled(cfg: &Config) -> bool {
    observation_active(cfg) && cfg.validation.paper_state_overrides
}

/// True se envio de TX deve ser barrado (paper / dry_run_only / env).
pub fn sends_forbidden(cfg: &Config) -> bool {
    paper_mode_active(cfg) || cfg.validation.dry_run_only || cfg.execution.dry_run
}

/// Paper validation ligado (env ou config).
pub fn paper_mode_active(cfg: &Config) -> bool {
    env_paper_flag() || cfg.validation.paper_enabled
}

/// Observação com `observe_min_spread` ativa — só paper/dry paths.
/// Fora disso, `observe_min_spread` tem **zero** efeito.
pub fn observation_active(cfg: &Config) -> bool {
    paper_mode_active(cfg) || cfg.validation.dry_run_only || cfg.execution.dry_run
}

/// `arbitrage.min_spread_percent` de execução (inalterado pela observação).
pub fn exec_min_spread_pct(cfg: &Config) -> f64 {
    cfg.arbitrage
        .min_spread_percent
        .parse::<f64>()
        .unwrap_or(0.5)
}

/// Resolve observe floor. Se `None` no config → igual ao exec (preserva default).
/// Se observe > exec, warn (config provavelmente invertida).
pub fn resolve_observe_min_spread(cfg: &Config) -> f64 {
    let exec = exec_min_spread_pct(cfg);
    let observe = cfg.validation.observe_min_spread.unwrap_or(exec);
    if observe > exec + 1e-12 {
        warn!(
            target: "paper_validation",
            observe_min_spread = observe,
            exec_min_spread = exec,
            "observe_min_spread > exec min_spread — config provavelmente errada"
        );
    }
    observe
}

/// Floor usado na **descoberta** de opps: observe só se `observation_active`, senão exec.
pub fn discovery_min_spread_pct(cfg: &Config) -> f64 {
    if observation_active(cfg) {
        resolve_observe_min_spread(cfg)
    } else {
        exec_min_spread_pct(cfg)
    }
}

/// Opp teria sido executável pelo critério de produção?
pub fn would_execute(spread_pct: f64, cfg: &Config) -> bool {
    spread_pct >= exec_min_spread_pct(cfg)
}

/// Rota suportada pelo `FlashloanExecutor` on-chain (QUICKSWAP/SUSHISWAP/UNISWAP_V3).
/// Curve aparece no radar mas **não** tem DexType no executor — skip no paper
/// para priorizar amostras úteis. Fee V3=100 ainda é observado (grava CSV com
/// abort A4) para medir quão frequente o bloqueio é no mid-band.
pub fn route_executor_supported(opp: &ArbitrageOpportunity) -> bool {
    opp.steps
        .0
        .iter()
        .all(|s| executor_dex_supported(&s.dex_name))
}

fn executor_dex_supported(dex: &str) -> bool {
    let n = dex
        .to_ascii_lowercase()
        .replace(' ', "")
        .replace('_', "")
        .replace("v2", "")
        .replace("v3", "");
    matches!(n.as_str(), "quickswap" | "sushiswap" | "uniswap")
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
    let tokens = decode(&[ParamType::Uint(256), ParamType::Uint(256)], data)
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
    /// Delta que o `eth_call` DEVE devolver: gross − prêmio do flashloan.
    ///
    /// Base correta do `erro_rel_pct`. `net_previsto_usd` embute gás (que o
    /// `eth_call` não cobra) e o adverse move opcional, então comparar contra ele
    /// media o próprio modelo de custo, não o erro de previsão do quote.
    pub delta_previsto_usd: f64,
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
    /// `spread >= exec_min_spread` — teria sido candidato a trade real.
    pub would_execute: bool,
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PaperAggregate {
    pub n_amostras: u64,
    pub n_reverts: u64,
    pub n_falsos_lucrativos: u64,
    pub n_would_execute: u64,
    pub n_reached_eth_call: u64,
    pub n_sim_ok: u64,
    pub erro_rel_pct_mean: Option<f64>,
    pub erro_rel_pct_p50: Option<f64>,
    pub erro_rel_pct_p95: Option<f64>,
}

impl PaperAggregate {
    pub fn from_samples(samples: &[PaperSample]) -> Self {
        let n = samples.len() as u64;
        let n_reverts = samples.iter().filter(|s| !s.sim_ok).count() as u64;
        let n_falsos = samples.iter().filter(|s| s.false_profitable).count() as u64;
        let n_would = samples.iter().filter(|s| s.would_execute).count() as u64;
        let n_eth = samples.iter().filter(|s| reached_eth_call(s)).count() as u64;
        let n_ok = samples.iter().filter(|s| s.sim_ok).count() as u64;

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
            n_would_execute: n_would,
            n_reached_eth_call: n_eth,
            n_sim_ok: n_ok,
            erro_rel_pct_mean: mean,
            erro_rel_pct_p50: percentile(&errs, 50.0),
            erro_rel_pct_p95: percentile(&errs, 95.0),
        }
    }

    pub fn log_summary(&self) {
        let fee100_discarded = crate::dex::fee100_best_discarded_count();
        let n_eth = self.n_reached_eth_call;
        let n_ok = self.n_sim_ok;
        info!(
            target: "paper_validation",
            "📊 PAPER SUMMARY | n={} would_execute={} reverts={} falsos_lucrativos={} erro_rel% mean={:?} p50={:?} p95={:?} | fee100_best_discarded={} low_liquidity_discarded={} triangular_leg_low_liquidity_discarded={} | n_reached_eth_call={} sim_ok={}",
            self.n_amostras,
            self.n_would_execute,
            self.n_reverts,
            self.n_falsos_lucrativos,
            self.erro_rel_pct_mean,
            self.erro_rel_pct_p50,
            self.erro_rel_pct_p95,
            fee100_discarded,
            crate::dex::liquidity::low_liquidity_discarded_count(),
            crate::core::arbitrage::triangular_leg_low_liquidity_discarded_count(),
            n_eth,
            n_ok,
        );
    }

    /// SUMMARY por par (calibração volátil): amostras, eth_call, sim_ok, reverts.
    pub fn log_summary_by_pair(samples: &[PaperSample]) {
        use std::collections::BTreeMap;
        let mut by: BTreeMap<String, Vec<&PaperSample>> = BTreeMap::new();
        for s in samples {
            by.entry(s.pair.clone()).or_default().push(s);
        }
        for (pair, xs) in by {
            let n = xs.len();
            let n_eth = xs.iter().filter(|s| reached_eth_call(s)).count();
            let n_ok = xs.iter().filter(|s| s.sim_ok).count();
            let n_rev = xs.iter().filter(|s| !s.sim_ok).count();
            let mut reasons: BTreeMap<String, usize> = BTreeMap::new();
            for s in &xs {
                if !s.sim_ok {
                    let r = s
                        .revert_reason
                        .as_deref()
                        .unwrap_or("unknown")
                        .chars()
                        .take(80)
                        .collect::<String>();
                    *reasons.entry(r).or_default() += 1;
                }
            }
            let reason_s = reasons
                .iter()
                .map(|(k, v)| format!("{}×{}", v, k))
                .collect::<Vec<_>>()
                .join(" | ");

            // Only REAL erro_rel from sim_ok samples
            let mut errs: Vec<f64> = xs
                .iter()
                .filter(|s| s.sim_ok)
                .filter_map(|s| s.erro_rel_pct)
                .collect();
            errs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
            let p50 = percentile(&errs, 50.0);
            let p95 = percentile(&errs, 95.0);

            let ok_detail: Vec<String> = xs
                .iter()
                .filter(|s| s.sim_ok)
                .map(|s| {
                    format!(
                        "net={:.4} real={:?} abs={:?} rel={:?}",
                        s.net_previsto_usd, s.profit_realizado_usd, s.erro_abs_usd, s.erro_rel_pct
                    )
                })
                .take(5)
                .collect();

            info!(
                target: "paper_validation",
                "📊 PAPER SUMMARY BY PAIR | pair={} n={} n_reached_eth_call={} sim_ok={} reverts={} | erro_rel% p50={:?} p95={:?} | reasons=[{}] | sim_ok_detail={:?}",
                pair,
                n,
                n_eth,
                n_ok,
                n_rev,
                p50,
                p95,
                reason_s,
                ok_detail,
            );
        }
    }

    /// SUMMARY por ciclo triangular (3 hops V3): rota, fees, net↔delta.
    pub fn log_summary_by_cycle(samples: &[PaperSample]) {
        let tris: Vec<&PaperSample> = samples.iter().filter(|s| is_triangular_sample(s)).collect();
        if tris.is_empty() {
            info!(
                target: "paper_validation",
                "📊 PAPER SUMMARY BY CYCLE | n_triangular=0 (nenhum ciclo 3-hop amostrado)"
            );
            return;
        }
        for s in &tris {
            info!(
                target: "paper_validation",
                "📊 PAPER SUMMARY BY CYCLE | route={} fee_tiers={} trade_usd={:.4} gross_rate_proxy={:.6} net_previsto_usd={:.6} profit_realizado_usd={:?} erro_abs={:?} erro_rel_pct={:?} revert_reason={:?} would_execute={} sim_ok={}",
                s.route,
                s.fee_tiers,
                s.trade_usd,
                if s.trade_usd > 1e-12 {
                    1.0 + (s.gross_previsto_usd / s.trade_usd)
                } else {
                    0.0
                },
                s.net_previsto_usd,
                s.profit_realizado_usd,
                s.erro_abs_usd,
                s.erro_rel_pct,
                s.revert_reason,
                s.would_execute,
                s.sim_ok,
            );
        }
        if let Some(first_ok) = tris.iter().find(|s| s.sim_ok && s.erro_rel_pct.is_some()) {
            info!(
                target: "paper_validation",
                "📊 PAPER SUMMARY BY CYCLE | first_sim_ok erro_rel_pct={:?} route={}",
                first_ok.erro_rel_pct,
                first_ok.route,
            );
        }
    }
}

/// Amostra triangular: 3 steps (2 separadores `|` na route) ou pair com 3 setas.
fn is_triangular_sample(s: &PaperSample) -> bool {
    s.route.matches('|').count() >= 2
        || s.pair.matches("->").count() >= 3
        || s.fee_tiers.matches(';').count() >= 2
}

/// eth_call chegou a rodar (não abort pré-encode fee100 / DEX ausente).
pub fn reached_eth_call(s: &PaperSample) -> bool {
    if s.sim_ok {
        return true;
    }
    let rr = s.revert_reason.as_deref().unwrap_or("");
    !(rr.contains("fee_tier=100")
        || rr.contains("ausente")
        || rr.contains("DEX não")
        || rr.contains("não suportada")
        || rr.contains("nao suportada"))
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
    would_execute: bool,
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

    // Base do erro: o que o eth_call deve devolver (gross − prêmio), não o net
    // de execução (que desconta gás — não cobrado em eth_call).
    let delta_previsto_usd =
        economics::expected_sim_delta_usd(opp.estimated_profit_usd, flashloan_fee_usd);

    let (erro_abs, erro_rel, false_profitable) = match profit_realizado_usd {
        Some(real) => {
            let abs = (delta_previsto_usd - real).abs();
            let rel = if delta_previsto_usd.abs() > 1e-12 {
                Some((abs / delta_previsto_usd.abs()) * 100.0)
            } else if real.abs() > 1e-12 {
                Some(100.0)
            } else {
                Some(0.0)
            };
            // "Falso lucrativo" continua julgado pelo net de EXECUÇÃO: prevíamos
            // lucro real e a simulação não entregou.
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
        delta_previsto_usd,
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
        would_execute,
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
                        PaperAggregate::log_summary_by_pair(&guard);
                        PaperAggregate::log_summary_by_cycle(&guard);
                    }
                    n
                };
                count2.store(n as u64, Ordering::Relaxed);
            }

            // flush summary on channel close
            let guard = agg2.lock().unwrap();
            if !guard.is_empty() {
                PaperAggregate::from_samples(&guard).log_summary();
                PaperAggregate::log_summary_by_pair(&guard);
                PaperAggregate::log_summary_by_cycle(&guard);
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
            "timestamp,pair,route,fee_tiers,trade_usd,net_previsto_usd,delta_previsto_usd,gross_previsto_usd,gas_usd,flashloan_fee_usd,profit_realizado_usd,erro_abs_usd,erro_rel_pct,block_number,sim_ok,revert_reason,false_profitable,would_execute"
        )?;
    }
    writeln!(
        f,
        "{},{},{},{},{:.8},{:.8},{:.8},{:.8},{:.8},{:.8},{},{},{},{},{},{},{},{}",
        s.timestamp,
        s.pair,
        escape_csv(&s.route),
        s.fee_tiers,
        s.trade_usd,
        s.net_previsto_usd,
        s.delta_previsto_usd,
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
        s.would_execute,
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
    let raw: Result<Value, _> = provider.request("alchemy_simulateAssetChanges", [tx]).await;
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

/// Endereço `from` para eth_call paper (nunca keypair).
/// Prioridade: `PAPER_FROM` env → `[validation].paper_from` →
/// `[wrapper].owner_address` → wallet do middleware (só endereço público).
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
    if let Some(ref owner) = cfg.wrapper.owner_address {
        if let Ok(addr) = owner.trim().parse::<Address>() {
            return addr;
        }
    }
    wallet
}

/// Confirma que resolvemos um endereço público — nunca uma chave.
pub fn paper_from_is_address_only(cfg: &Config, wallet: Address) -> Address {
    let a = resolve_paper_from(cfg, wallet);
    a
}

pub async fn current_block_number(client: &AppMiddleware) -> Result<u64> {
    Ok(client.get_block_number().await?.as_u64())
}

pub fn block_id(n: u64) -> BlockId {
    BlockId::Number(BlockNumber::Number(n.into()))
}

/// Erros típicos de RPC sem archive / estado histórico ausente.
pub fn is_archive_state_error(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("missing trie node")
        || e.contains("historical state")
        || e.contains("state is not available")
        || e.contains("unknown block")
        || e.contains("header not found")
        || e.contains("cannot query unfinalized data")
        || e.contains("pruned history")
        || e.contains("ancient") && e.contains("not available")
        || e.contains("archive") && (e.contains("required") || e.contains("unavailable"))
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
        would_execute = s.would_execute,
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
    fn decode_nested_ethers_string_data() {
        // Formato que o provider devolveu no paper run
        let err = r#"(code: 3, message: execution reverted: Invalid initiator, data: Some(String("0x08c379a000000000000000000000000000000000000000000000000000000000000000200000000000000000000000000000000000000000000000000000000000000011496e76616c696420696e69746961746f72000000000000000000000000000000")))"#;
        let msg = decode_revert_message(err);
        assert!(msg.contains("Invalid initiator"), "msg={msg}");
    }

    #[test]
    fn decode_error_string_fixture_not_profitable() {
        let body = encode(&[Token::String("Not profitable".into())]);
        let hex = format!("08c379a0{}", hex::encode(&body));
        let msg = decode_revert_data_hex(&hex);
        assert!(msg.contains("Not profitable"), "msg={msg}");
        assert!(msg.starts_with("Error("), "msg={msg}");
    }

    #[test]
    fn decode_panic_fixture_overflow() {
        let body = encode(&[Token::Uint(U256::from(0x11u64))]);
        let hex = format!("4e487b71{}", hex::encode(&body));
        let msg = decode_revert_data_hex(&hex);
        assert!(
            msg.contains("overflow") || msg.contains("0x11"),
            "msg={msg}"
        );
    }

    #[test]
    fn decode_from_provider_error_string() {
        let body = encode(&[Token::String("Not executor".into())]);
        let hex = format!("08c379a0{}", hex::encode(&body));
        let err = format!("execution reverted: data: 0x{hex}");
        let msg = decode_revert_message(&err);
        assert!(msg.contains("Not executor"), "msg={msg}");
    }

    #[test]
    fn resolve_paper_from_prefers_config_over_wallet() {
        std::env::remove_var(ENV_PAPER_FROM);
        let mut cfg = Config::default();
        cfg.validation.paper_from = "0x1111111111111111111111111111111111111111".into();
        let wallet: Address = "0x2222222222222222222222222222222222222222"
            .parse()
            .unwrap();
        let from = resolve_paper_from(&cfg, wallet);
        assert_eq!(
            format!("{:?}", from).to_ascii_lowercase(),
            "0x1111111111111111111111111111111111111111"
        );
    }

    #[test]
    fn state_override_json_has_token_key() {
        let token: Address = "0x2791bca1f2de4661ed88a30c99a7a9449aa84174"
            .parse()
            .unwrap();
        let holder: Address = "0x152Aa7ecC490860115C4d1369a19C970f9e9eFFf"
            .parse()
            .unwrap();
        let ovr = erc20_balance_state_override(token, holder, U256::from(1_000_000u64));
        let key = format!("{:?}", token);
        assert!(
            ovr.get(&key).is_some()
                || ovr
                    .as_object()
                    .unwrap()
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case(&key))
        );
    }

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
                delta_previsto_usd: 1.0,
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
                would_execute: false,
            },
            PaperSample {
                timestamp: "t".into(),
                pair: "A-B".into(),
                route: "r".into(),
                fee_tiers: "-".into(),
                trade_usd: 100.0,
                net_previsto_usd: 0.5,
                delta_previsto_usd: 0.5,
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
                would_execute: true,
            },
        ];
        let agg = PaperAggregate::from_samples(&samples);
        assert_eq!(agg.n_amostras, 2);
        assert_eq!(agg.n_falsos_lucrativos, 1);
        assert_eq!(agg.n_would_execute, 1);
        assert_eq!(agg.erro_rel_pct_p50, Some(20.0));
    }

    #[test]
    fn build_sample_marks_false_profitable() {
        let mut opp = ArbitrageOpportunity::default();
        opp.pair = "USDT-WMATIC".into();
        opp.net_profit_usd = 0.5;
        opp.estimated_profit_usd = 1.0;
        opp.estimated_volume_usd = 100.0;
        let s = build_sample(&opp, 99, 0.05, Some(0.0), true, None, false);
        assert!(s.false_profitable);
        assert!(!s.would_execute);
        assert!(s.erro_abs_usd.unwrap() > 0.0);
    }

    #[test]
    fn observe_min_spread_noop_when_paper_off() {
        std::env::remove_var(ENV_PAPER_VALIDATION);
        let mut cfg = Config::default();
        cfg.execution.dry_run = false;
        cfg.validation.paper_enabled = false;
        cfg.validation.dry_run_only = false;
        cfg.arbitrage.min_spread_percent = "0.50".into();
        cfg.validation.observe_min_spread = Some(0.05);
        // Paper OFF → discovery == exec, observe ignorado
        assert!(!observation_active(&cfg));
        assert!((discovery_min_spread_pct(&cfg) - 0.50).abs() < 1e-12);
        assert!((exec_min_spread_pct(&cfg) - 0.50).abs() < 1e-12);
    }

    #[test]
    fn observe_mid_band_would_execute_false() {
        std::env::set_var(ENV_PAPER_VALIDATION, "1");
        let mut cfg = Config::default();
        cfg.arbitrage.min_spread_percent = "0.50".into();
        cfg.validation.observe_min_spread = Some(0.05);
        assert!(observation_active(&cfg));
        assert!((discovery_min_spread_pct(&cfg) - 0.05).abs() < 1e-12);
        // Entre observe e exec
        assert!(!would_execute(0.08, &cfg));
        assert!(would_execute(0.60, &cfg));
        assert!(sends_forbidden(&cfg)); // ainda bloqueia envio
        std::env::remove_var(ENV_PAPER_VALIDATION);
    }

    #[test]
    fn route_executor_skips_curve() {
        let mut ok = ArbitrageOpportunity::default();
        ok.steps.0 = vec![
            crate::core::types::ArbitrageStep {
                dex_name: "SushiSwap".into(),
                token_in: "USDT".into(),
                token_out: "WMATIC".into(),
                ..Default::default()
            },
            crate::core::types::ArbitrageStep {
                dex_name: "UniswapV3".into(),
                token_in: "WMATIC".into(),
                token_out: "USDT".into(),
                v3_fee_tier: Some(500),
                ..Default::default()
            },
        ];
        assert!(route_executor_supported(&ok));

        let mut curve = ok.clone();
        curve.steps.0[0].dex_name = "Curve".into();
        assert!(!route_executor_supported(&curve));
    }

    #[test]
    fn observe_above_exec_warns_but_resolves() {
        std::env::remove_var(ENV_PAPER_VALIDATION);
        let mut cfg = Config::default();
        cfg.validation.paper_enabled = true;
        cfg.arbitrage.min_spread_percent = "0.10".into();
        cfg.validation.observe_min_spread = Some(0.50); // invertido
        let v = resolve_observe_min_spread(&cfg);
        assert!((v - 0.50).abs() < 1e-12);
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

    #[test]
    fn reached_eth_call_filters_pre_encode_aborts() {
        let mut s = PaperSample {
            timestamp: String::new(),
            pair: "USDC-WETH".into(),
            route: String::new(),
            fee_tiers: "500".into(),
            trade_usd: 100.0,
            net_previsto_usd: 1.0,
            delta_previsto_usd: 1.0,
            gross_previsto_usd: 2.0,
            gas_usd: 0.1,
            flashloan_fee_usd: 0.05,
            profit_realizado_usd: None,
            erro_abs_usd: None,
            erro_rel_pct: None,
            block_number: 1,
            sim_ok: false,
            revert_reason: Some("fee_tier=100 unsupported".into()),
            false_profitable: false,
            would_execute: false,
        };
        assert!(!reached_eth_call(&s));
        s.revert_reason = Some(r#"Error("Not profitable")"#.into());
        assert!(reached_eth_call(&s));
        s.sim_ok = true;
        s.revert_reason = None;
        assert!(reached_eth_call(&s));
    }
}
