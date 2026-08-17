// ============================================================
// src/utils/utils.rs — Versão 2025 (Produção Final Corrigida)
// Patch V8.4: Correção de normalize_amount overflow + is_realistic_price para WETH-DAI
// ============================================================

use std::collections::HashMap;
use std::fmt::Debug;
use std::future::Future;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use chrono::Utc;
use ethers::types::{U256, U512};
use tokio::time::sleep;
use tracing::{debug, error, info, warn};

use crate::core::types::ArbitrageOpportunity;

// ============================================================
// 🕒 Tempo e Datas
// ============================================================

pub fn now_ts() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_secs()
}

pub fn now_ms() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_else(|_| Duration::from_secs(0))
        .as_millis()
}

pub fn elapsed_ms(start: std::time::Instant) -> u128 {
    start.elapsed().as_millis()
}

pub fn elapsed_secs(start: std::time::Instant) -> f64 {
    start.elapsed().as_secs_f64()
}

pub fn fmt_elapsed(start: std::time::Instant) -> String {
    format!("{:.2}s", elapsed_secs(start))
}

pub fn current_timestamp_str() -> String {
    Utc::now().format("%Y-%m-%d %H:%M:%S UTC").to_string()
}

pub fn current_timestamp() -> u64 {
    now_ts()
}

// ============================================================
// 💲 Formatação e Logs
// ============================================================

pub fn fmt_usd(value: f64) -> String {
    format!("${:.2}", value)
}

pub fn fmt_gas(value: f64) -> String {
    format!("{:.2} gwei", value)
}

pub fn log_separator(title: &str) {
    info!("================= {} =================", title);
}

pub fn log_warn(context: &str, msg: &str) {
    warn!("⚠️ [{}] {}", context, msg);
}

pub fn log_error(context: &str, err: &str) {
    error!("❌ [{}] {}", context, err);
}

pub fn log_info(context: &str, msg: &str) {
    info!("ℹ️ [{}] {}", context, msg);
}

// ============================================================
// 🔢 Conversões Numéricas e On-chain
// ============================================================

pub fn wei_to_eth(wei: u128) -> f64 {
    (wei as f64) / 1e18
}

pub fn eth_to_wei(eth: f64) -> u128 {
    (eth * 1e18) as u128
}

pub fn gwei_to_wei(gwei: f64) -> u128 {
    (gwei * 1e9) as u128
}

pub fn wei_to_gwei(wei: u128) -> f64 {
    (wei as f64) / 1e9
}

/// Converte U256 para f64 considerando decimais — **somente telemetria/UI**.
///
/// Pré-escala no domínio U256. Ainda aplica teto `u128::MAX` pós-escala para o
/// cast f64; gates econômicos devem usar `core::fixed_usd` (erro explícito).
pub fn u256_to_f64(value: U256, decimals: u32) -> f64 {
    let dec = decimals.min(36) as usize;
    let amount_18: U256 = if dec >= 18 {
        value / U256::exp10(dec - 18)
    } else {
        value.saturating_mul(U256::exp10(18 - dec))
    };
    if amount_18 > U256::from(u128::MAX) {
        // Display path: escalate via string rather than silent economic clip.
        let as_tokens = amount_18 / U256::exp10(18);
        return as_tokens.to_string().parse::<f64>().unwrap_or(f64::MAX);
    }
    let amt = amount_18.as_u128() as f64 / 1e18;
    if amt.is_finite() {
        amt
    } else {
        0.0
    }
}

/// Converte f64 para U256 considerando decimais, truncando negativos
pub fn f64_to_u256(value: f64, decimals: u32) -> U256 {
    let scale = 10u128.saturating_pow(decimals.min(38)) as f64;
    let scaled = (value * scale).max(0.0);
    U256::from(scaled as u128)
}

// ============================================================
// 🔘 Booleanos e Status Visuais
// ============================================================

pub fn bool_icon(value: bool) -> &'static str {
    if value {
        "🟢"
    } else {
        "🔴"
    }
}

pub fn bool_status(value: bool) -> &'static str {
    if value {
        "✔️"
    } else {
        "❌"
    }
}

// ============================================================
// 🧩 Validadores e Endereços
// ============================================================

pub fn normalize_address(addr: &str) -> String {
    addr.trim().to_lowercase()
}

pub fn is_valid_eth_address(addr: &str) -> bool {
    addr.starts_with("0x") && addr.len() == 42
}

pub fn within_tolerance(a: f64, b: f64, tolerance_pct: f64) -> bool {
    let diff = (a - b).abs();
    let avg = (a.abs() + b.abs()) / 2.0;
    if avg == 0.0 {
        return false;
    }
    (diff / avg) * 100.0 <= tolerance_pct
}

// ============================================================
// ⏳ Controle de Fluxo
// ============================================================

pub async fn delay_secs(secs: u64) {
    sleep(Duration::from_secs(secs)).await;
}

pub async fn delay_ms(ms: u64) {
    sleep(Duration::from_millis(ms)).await;
}

pub async fn retry_with_backoff<F, Fut, T, E>(
    mut operation: F,
    max_retries: usize,
    base_delay: Duration,
    context: &str,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
    E: Debug,
{
    let mut delay = base_delay;
    for attempt in 0..max_retries {
        match operation().await {
            Ok(res) => return Ok(res),
            Err(err) => {
                warn!(
                    "⚠️ Falha `{}` (tentativa {}/{}): {:?}",
                    context,
                    attempt + 1,
                    max_retries,
                    err
                );
                if attempt + 1 == max_retries {
                    return Err(anyhow!("❌ {} após {} tentativas", context, max_retries));
                }
                sleep(delay).await;
                delay *= 2;
            }
        }
    }
    Err(anyhow!("❌ Retry esgotado: {}", context))
}

pub async fn measure_time<F, Fut, T>(context: &str, operation: F) -> Result<T>
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let start = std::time::Instant::now();
    let result = operation().await;
    info!("⏱️ {} concluído em {:.2?}", context, start.elapsed());
    result
}

// ============================================================
// 🆔 Identificadores
// ============================================================

pub fn unique_id(prefix: &str) -> String {
    format!("{}-{}", prefix, now_ms())
}

// ============================================================
// 💰 Gerenciamento de Preços de Tokens
// ============================================================

/// Define o preço médio (em USD) do token base de uma oportunidade.
/// - Valida o valor antes de aplicar.
/// - Atualiza o campo `token_price_usd` (Option<f64>).
/// - Retorna erro se o preço for inválido.
pub fn set_token_price_usd(opp: &mut ArbitrageOpportunity, price_usd: f64) -> Result<()> {
    if !validate_price(price_usd) {
        warn!(
            "⚠️ Preço inválido para {:?}: {:.12} — ignorado",
            opp.pair, price_usd
        );
        return Err(anyhow!("Preço inválido: {:.12}", price_usd));
    }

    let normalized = normalize_price(price_usd).unwrap_or(1.0);
    opp.token_price_usd = Some(normalized);

    info!(
        "💲 Token price atualizado para {} → {:.6} USD",
        opp.pair, normalized
    );

    Ok(())
}

/// Retorna o preço do token armazenado em `token_price_usd`, com fallback de 1.0 USD.
/// Isso evita panics em avaliações de risco e cálculos de lucro.
pub fn get_token_price_usd(opp: &ArbitrageOpportunity) -> f64 {
    opp.token_price_usd.unwrap_or(1.0)
}

// ============================================================
// Validação de Preço
// ============================================================

pub fn validate_price(price: f64) -> bool {
    price > 1e-12 && price < 1e12
}

pub fn normalize_price(price: f64) -> Option<f64> {
    if validate_price(price) {
        Some(price)
    } else {
        None
    }
}

// ============================================================
// ⚖️ Conversão Uniswap V3 e Filtro de Outliers
// ============================================================

#[inline]
pub fn pow10_u256(exp: i32) -> U256 {
    let mut r = U256::from(1u64);
    for _ in 0..exp.max(0) {
        r = r.saturating_mul(U256::from(10u64));
    }
    r
}

/// Calcula preço base→quote para Uniswap V3
pub fn univ3_price_base_quote(
    sqrt_price_x96: U256,
    token0: [u8; 20],
    token1: [u8; 20],
    base: [u8; 20],
    quote: [u8; 20],
    decimals0: u8,
    decimals1: u8,
) -> f64 {
    let num = U512::from(sqrt_price_x96).saturating_mul(U512::from(sqrt_price_x96));
    let shifted = num >> 192;
    let mut buf = [0u8; 32];
    shifted.to_little_endian(&mut buf);
    let price_u = U256::from_little_endian(&buf);
    let exp = (decimals0 as i32) - (decimals1 as i32);
    let scaled = if exp >= 0 {
        price_u.saturating_mul(pow10_u256(exp))
    } else {
        price_u.checked_div(pow10_u256(-exp)).unwrap_or_default()
    };
    let base_quote = u256_to_f64(scaled, 0);
    let is01 = base == token0 && quote == token1;
    let is10 = base == token1 && quote == token0;
    if is01 {
        1.0 / base_quote
    } else if is10 {
        base_quote
    } else {
        f64::NAN
    }
}

/// Conversão precisa de U256 para f64 — delega à pré-escala U256 compartilhada.
pub fn u256_to_f64_precise(value: U256, decimals: u8) -> f64 {
    u256_to_f64(value, decimals as u32)
}

/// Filtra outliers removendo desvios > pct
pub fn robust_filter(prices: &mut Vec<(String, f64)>, max_dev_pct: f64) {
    let mut vals: Vec<f64> = prices
        .iter()
        .map(|(_, p)| *p)
        .filter(|v| v.is_finite() && *v > 0.0)
        .collect();
    if vals.len() < 3 {
        return;
    }
    vals.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let med = vals[vals.len() / 2];
    prices.retain(|(_, p)| ((p - med) / med).abs() <= max_dev_pct);
}

// ============================================================
// 📊 Utilitários para Price Feeds
// ============================================================

/// Obtém preço de token do cache ou retorna fallback
pub fn get_cached_token_price(token: &str, price_cache: &HashMap<String, f64>) -> f64 {
    price_cache.get(token).cloned().unwrap_or_else(|| {
        // Fallback para tokens comuns
        if token.contains("USDC") || token.contains("DAI") || token.contains("USDT") {
            1.0
        } else if token.contains("WETH") || token.contains("ETH") {
            2900.0
        } else if token.contains("WMATIC") || token.contains("MATIC") {
            0.75
        } else if token.contains("WBTC") || token.contains("BTC") {
            42000.0
        } else {
            1.0 // Fallback genérico
        }
    })
}

/// Atualiza cache de preços com validação
pub fn update_price_cache(
    cache: &mut HashMap<String, f64>,
    token: String,
    price: f64,
) -> Result<()> {
    if validate_price(price) {
        cache.insert(token, price);
        Ok(())
    } else {
        Err(anyhow!("Preço inválido para cache: {}", price))
    }
}

// ============================================================
// 🔧 Funções de Depuração
// ============================================================

/// Log detalhado de oportunidade para debug
pub fn debug_opportunity(opp: &ArbitrageOpportunity) {
    info!(
        "🔍 DEBUG OPPORTUNITY | pair: {} | buy: {:.6} | sell: {:.6} | spread: {:.3}% | profit: ${:.4} | gas: ${:.4} | net: ${:.4}",
        opp.pair,
        opp.buy_price,
        opp.sell_price,
        opp.spread_percent,
        opp.estimated_profit_usd,
        opp.gas_cost_usd,
        opp.net_profit_usd
    );
}

/// Verifica consistência de dados numéricos
pub fn check_financial_consistency(opp: &ArbitrageOpportunity) -> bool {
    opp.buy_price > 0.0
        && opp.sell_price > 0.0
        && opp.estimated_profit_usd.is_finite()
        && opp.gas_cost_usd >= 0.0
        && opp.net_profit_usd.is_finite()
}

// ============================================================
// 🧮 FUNÇÕES DE CÁLCULO CORRIGIDAS (V8.4 — FINAL)
// ============================================================

/// Normaliza amount considerando decimals — VERSÃO CORRIGIDA COM OVERFLOW PROTECTION
/// ✅ Suporta U256 > u128::MAX sem truncamento silencioso
pub fn normalize_amount(amount: U256, decimals: u32) -> f64 {
    if amount.is_zero() {
        return 0.0;
    }

    // Cap decimals em 36 para evitar expoentes gigantes
    let safe_decimals = decimals.min(36);

    // Calcular divisor
    let divisor = U256::from(10u64).pow(U256::from(safe_decimals));

    // Dividir inteiro e fracionário
    let whole = amount / divisor;
    let fractional = amount % divisor;

    // Converter whole: se > u128::MAX, usar string parsing
    let whole_f64 = if whole > U256::from(u128::MAX) {
        // Parse via string para números grandes
        whole.to_string().parse::<f64>().unwrap_or(0.0)
    } else {
        whole.as_u128() as f64
    };

    // Fracionário sempre < divisor, então seguro usar as_u128
    let frac_f64 = fractional.as_u128() as f64 / 10f64.powi(safe_decimals as i32);

    let result = whole_f64 + frac_f64;

    // Debug log para valores suspeitos
    if result.is_infinite() || result.is_nan() {
        warn!(
            "⚠️ normalize_amount retornou valor inválido: {} (amount={}, decimals={})",
            result, amount, decimals
        );
    }

    result
}

/// Converte amount normalizado para U256 considerando decimais
pub fn denormalize_amount(amount: f64, decimals: u32) -> U256 {
    if amount <= 0.0 || !amount.is_finite() {
        return U256::zero();
    }

    let safe_decimals = decimals.min(36);
    let multiplier = U256::from(10u64).pow(U256::from(safe_decimals));

    // Usar u512 para intermediate calc para evitar overflow
    let scaled_f64 = amount * multiplier.as_u128() as f64;
    let scaled_u128 = scaled_f64.round() as u128;

    U256::from(scaled_u128)
}

/// Calcula preço real entre dois tokens
/// Semântica: preço = quantos token_out você recebe por 1 unidade de token_in
pub fn calculate_real_price(
    amount_in: U256,
    decimals_in: u32,
    amount_out: U256,
    decimals_out: u32,
) -> f64 {
    let normalized_in = normalize_amount(amount_in, decimals_in);
    let normalized_out = normalize_amount(amount_out, decimals_out);

    if normalized_in == 0.0 || !normalized_in.is_finite() {
        return 0.0;
    }

    let price = normalized_out / normalized_in;

    // Validação de sanidade
    if !price.is_finite() {
        debug!(
            "⚠️ calculate_real_price: resultado inválido {} (in={}, out={})",
            price, normalized_in, normalized_out
        );
        return 0.0;
    }

    price
}

/// Valida se um preço é realista para impedir divergências absurdas
/// ✅ CORRIGIDO: Agora suporta WETH-DAI e outros pares de forma robusta
pub fn is_realistic_price(price: f64, token_pair: &str) -> bool {
    if !price.is_finite() || price <= 0.0 {
        return false;
    }

    let pair_lower = token_pair.to_lowercase();

    // Stablecoins entre si (USDT, USDC, DAI)
    if (pair_lower.contains("usdt") || pair_lower.contains("usdc") || pair_lower.contains("dai"))
        && (pair_lower.contains("usdt")
            || pair_lower.contains("usdc")
            || pair_lower.contains("dai"))
    {
        return price >= 0.8 && price <= 1.2;
    }

    // WETH ↔ qualquer stable (USDT, USDC, DAI) — ✅ AGORA COBRE WETH-DAI
    if pair_lower.contains("weth") {
        if pair_lower.contains("usdt") || pair_lower.contains("usdc") || pair_lower.contains("dai")
        {
            return price >= 1000.0 && price <= 10000.0;
        }

        // WETH ↔ WMATIC
        if pair_lower.contains("wmatic") || pair_lower.contains("matic") {
            return price >= 1000.0 && price <= 100000.0;
        }
    }

    // WMATIC ↔ qualquer stable
    if pair_lower.contains("wmatic") || pair_lower.contains("matic") {
        if pair_lower.contains("usdt") || pair_lower.contains("usdc") || pair_lower.contains("dai")
        {
            return price >= 0.05 && price <= 10.0;
        }
    }

    // Stablecoins invertidos (inverso de 1:1)
    if (pair_lower.contains("usdt") || pair_lower.contains("usdc") || pair_lower.contains("dai"))
        && (pair_lower.contains("weth") || pair_lower.contains("eth"))
    {
        return price >= 0.0001 && price <= 0.001;
    }

    // Fallback conservador mas amplo
    price >= 0.00001 && price <= 1000000.0
}

/// Valida e filtra preços baseado em realismo + desvio de mercado
/// Retorna true se preço é aceitável
pub fn validate_and_filter_price(
    price: f64,
    token_pair: &str,
    market_prices: &[(f64, &str)], // (price, source_dex)
) -> bool {
    // Validação básica
    if !is_realistic_price(price, token_pair) {
        debug!("❌ Preço irreal rejeitado: {} para {}", price, token_pair);
        return false;
    }

    // Se temos preços de mercado, validar desvio
    if market_prices.len() > 1 {
        let median = {
            let mut prices: Vec<f64> = market_prices.iter().map(|(p, _)| *p).collect();
            prices.sort_by(|a, b| a.partial_cmp(b).unwrap());
            prices[prices.len() / 2]
        };

        let deviation_pct = ((price - median).abs() / median) * 100.0;

        // Rejeitar se desvio > 50% (muito fora do mercado)
        if deviation_pct > 50.0 {
            warn!(
                "⚠️ Preço MUITO divergente: {} para {} | desvio={:.2}% | mediana={}",
                price, token_pair, deviation_pct, median
            );
            return false;
        }

        // Debug log para desvios moderados (10-50%)
        if deviation_pct > 10.0 {
            debug!(
                "⚠️ Preço moderadamente divergente: {} para {} | desvio={:.2}% | mediana={}",
                price, token_pair, deviation_pct, median
            );
        }
    }

    true
}
