//! Gate de liquidez on-chain (TVL proxy via ERC20 `balanceOf(pool)`).
//!
//! **Threshold efetivo:** máximo entre `config.arbitrage.min_liquidity`, override
//! do venue e `arbitrage.route_validation.min_liquidity_per_hop` (USD).
//! A constante morta `MIN_POOL_LIQUIDITY_USD` no radar foi removida — não duplicar.
//!
//! **Por que `balanceOf` e não `getReserves`:**
//! - Uniforme V2 + V3 (V3 não tem getReserves).
//! - Encaixa no mesmo multicall batch (2 calls/pool).
//! - Para V3 é um **proxy mínimo** (soma balances ≥ liquidez usável na tick
//!   range); serve para cortar dust, não como depth executável real.
//!
//! Aplicado no radar **antes** de emitir preço no mapa (junto de
//! `prune_non_reciprocal`), após o multicall de quotes — um multicall extra
//! **por batch** de balances, nunca N RPCs seriais por par.

use crate::{
    config::{token_cache::TokenCache, Config},
    contracts::ERC20,
    dex::TokenPairPrice,
    infra::price_feed::PRICE_FEED,
    AppMiddleware,
};

use anyhow::Result;
use ethers::{
    abi::Token,
    contract::Multicall,
    types::{Address, U256},
};
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use tracing::{debug, info, warn};

/// Contador global: pares descartados por TVL < threshold (análogo a fee100).
static LOW_LIQUIDITY_DISCARDED: AtomicU64 = AtomicU64::new(0);

/// Cache `dex:TOKENA-TOKENB` (canônico) → endereço do pool/pair.
static POOL_ADDR_CACHE: Lazy<RwLock<HashMap<String, Address>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Decimals cacheados por endereço de token (evita re-resolve).
static DECIMALS_CACHE: Lazy<RwLock<HashMap<Address, u8>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Threshold global: `arbitrage.min_liquidity` do config (USD).
pub fn min_pool_liquidity_usd(cfg: &Config) -> f64 {
    cfg.arbitrage
        .min_liquidity
        .parse::<f64>()
        .unwrap_or(5_000.0)
        .max(0.0)
}

/// Threshold do venue: `[[dex]].liquidity_threshold_usd`, caindo no global.
///
/// Pools V3 concentrados e pools de stable toleram TVL diferente de um pool V2
/// genérico, daí o override por venue. A chave já existia no TOML mas não era
/// lida — todo venue usava o global.
pub fn min_pool_liquidity_usd_for_dex(cfg: &Config, dex_name: &str) -> f64 {
    let dex_threshold = cfg.dex
        .iter()
        .find(|d| d.name.eq_ignore_ascii_case(dex_name))
        .and_then(|d| d.liquidity_threshold_usd)
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or_else(|| min_pool_liquidity_usd(cfg));

    // Toda perna precisa passar tanto o threshold do venue/global quanto o
    // mínimo explícito da validação de rota. Não existe bypass por caminho.
    let route_threshold = cfg
        .arbitrage
        .route_validation
        .min_liquidity_per_hop
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .unwrap_or(0.0);

    dex_threshold.max(route_threshold)
}

pub fn low_liquidity_discarded_count() -> u64 {
    LOW_LIQUIDITY_DISCARDED.load(Ordering::Relaxed)
}

pub fn reset_low_liquidity_discarded_count() {
    LOW_LIQUIDITY_DISCARDED.store(0, Ordering::Relaxed);
}

fn note_low_liquidity_discarded(n: u64) {
    if n > 0 {
        LOW_LIQUIDITY_DISCARDED.fetch_add(n, Ordering::Relaxed);
    }
}

/// Replay scan: incrementa o mesmo counter do B2 live.
pub fn note_low_liquidity_discarded_pub(n: u64) {
    note_low_liquidity_discarded(n);
}

fn pool_cache_key(dex: &str, token_a: &str, token_b: &str, fee_tier: u32) -> String {
    let a = token_a.to_ascii_uppercase();
    let b = token_b.to_ascii_uppercase();
    let (x, y) = if a <= b { (a, b) } else { (b, a) };
    format!("{}:{}-{}:{}", dex, x, y, fee_tier)
}

pub fn cached_pool_address(dex: &str, token_a: &str, token_b: &str, fee_tier: u32) -> Option<Address> {
    let key = pool_cache_key(dex, token_a, token_b, fee_tier);
    POOL_ADDR_CACHE.read().ok()?.get(&key).copied()
}

pub fn cache_pool_address(dex: &str, token_a: &str, token_b: &str, fee_tier: u32, pool: Address) {
    if pool.is_zero() {
        return;
    }
    let key = pool_cache_key(dex, token_a, token_b, fee_tier);
    if let Ok(mut g) = POOL_ADDR_CACHE.write() {
        g.insert(key, pool);
    }
}

/// TVL proxy USD = Σ (balance_i / 10^dec_i * price_usd_i).
/// Puro / testável sem RPC.
pub fn pool_tvl_usd_from_balances(
    bal_a: U256,
    decimals_a: u8,
    price_a_usd: f64,
    bal_b: U256,
    decimals_b: u8,
    price_b_usd: f64,
) -> f64 {
    raw_to_usd(bal_a, decimals_a, price_a_usd) + raw_to_usd(bal_b, decimals_b, price_b_usd)
}

fn raw_to_usd(raw: U256, decimals: u8, price_usd: f64) -> f64 {
    if price_usd <= 0.0 || !price_usd.is_finite() {
        return 0.0;
    }
    let dec = decimals.min(36) as i32;
    let denom = 10f64.powi(dec);
    let amt = {
        let capped = raw.min(U256::from(u128::MAX));
        capped.as_u128() as f64 / denom
    };
    amt * price_usd
}

/// True se TVL >= threshold (gate passa).
pub fn passes_liquidity_gate(tvl_usd: f64, min_usd: f64) -> bool {
    tvl_usd.is_finite() && tvl_usd >= min_usd
}

async fn token_price_usd(symbol: &str) -> f64 {
    match PRICE_FEED.get_price(symbol).await {
        Ok(p) if p > 0.0 && p.is_finite() => p,
        _ => {
            // Stables comuns — fallback local se feed falhar.
            match symbol.to_ascii_uppercase().as_str() {
                "USDT" | "USDC" | "DAI" | "USDC.E" | "USDT.E" => 1.0,
                _ => 0.0,
            }
        }
    }
}

/// Usa decimals do TokenCache (já resolvidos fora do hot path). Sem RPC extra.
fn cached_decimals(token: Address, hint: u8) -> u8 {
    if let Ok(g) = DECIMALS_CACHE.read() {
        if let Some(d) = g.get(&token) {
            return *d;
        }
    }
    if let Ok(mut g) = DECIMALS_CACHE.write() {
        g.insert(token, hint);
    }
    hint
}

/// Filtra preços cujo pool tem TVL proxy < `min_usd`.
///
/// 1) Resolve/caches pool address (getPair/getPool — miss só; paralelo via join_all).
/// 2) Um Multicall de `balanceOf` (2× por pool) — **um** round-trip por batch.
/// 3) Converte via `PRICE_FEED` (mesmo feed do quote sizing).
///
/// **Fail-open:** se não der para medir (pool miss / multicall / sem preço),
/// mantém o quote — só descarta quando TVL foi medido e está < threshold.
pub async fn filter_token_prices_by_liquidity<F, Fut>(
    client: Arc<AppMiddleware>,
    token_cache: &TokenCache,
    prices: Vec<TokenPairPrice>,
    min_usd: f64,
    mut resolve_pool: F,
) -> Result<Vec<TokenPairPrice>>
where
    F: FnMut(String, Address, Address, u32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Address>>>,
{
    if prices.is_empty() || min_usd <= 0.0 {
        return Ok(prices);
    }

    struct Item {
        tp: TokenPairPrice,
        pool: Address,
        addr_a: Address,
        addr_b: Address,
        dec_a: u8,
        dec_b: u8,
    }

    struct Pending {
        tp: TokenPairPrice,
        addr_a: Address,
        addr_b: Address,
        dec_a: u8,
        dec_b: u8,
        fee: u32,
    }

    let mut items: Vec<Item> = Vec::with_capacity(prices.len());
    let mut pending: Vec<Pending> = Vec::new();
    // Quotes que não pudemos medir → fail-open (mantém).
    let mut keep_unmeasured: Vec<TokenPairPrice> = Vec::new();
    let mut discarded = 0u64;

    for tp in prices {
        let (Some(info_a), Some(info_b)) = (
            token_cache.get_by_symbol(&tp.token_a).await,
            token_cache.get_by_symbol(&tp.token_b).await,
        ) else {
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", tp.token_a, tp.token_b),
                "fail-open: token_cache miss"
            );
            keep_unmeasured.push(tp);
            continue;
        };

        let fee = tp.fee_tier_bps;
        let dec_a = cached_decimals(info_a.address, info_a.decimals);
        let dec_b = cached_decimals(info_b.address, info_b.decimals);

        if let Some(p) = cached_pool_address(&tp.dex_name, &tp.token_a, &tp.token_b, fee) {
            items.push(Item {
                tp,
                pool: p,
                addr_a: info_a.address,
                addr_b: info_b.address,
                dec_a,
                dec_b,
            });
        } else {
            pending.push(Pending {
                tp,
                addr_a: info_a.address,
                addr_b: info_b.address,
                dec_a,
                dec_b,
                fee,
            });
        }
    }

    if !pending.is_empty() {
        let mut futs = Vec::with_capacity(pending.len());
        for p in &pending {
            futs.push(resolve_pool(
                p.tp.dex_name.clone(),
                p.addr_a,
                p.addr_b,
                p.fee,
            ));
        }
        let resolved = futures::future::join_all(futs).await;
        for (p, res) in pending.into_iter().zip(resolved) {
            match res {
                Ok(Some(pool)) if !pool.is_zero() => {
                    cache_pool_address(&p.tp.dex_name, &p.tp.token_a, &p.tp.token_b, p.fee, pool);
                    items.push(Item {
                        tp: p.tp,
                        pool,
                        addr_a: p.addr_a,
                        addr_b: p.addr_b,
                        dec_a: p.dec_a,
                        dec_b: p.dec_b,
                    });
                }
                _ => {
                    debug!(
                        target: "liquidity",
                        pair = %format!("{}-{}", p.tp.token_a, p.tp.token_b),
                        dex = %p.tp.dex_name,
                        fee = p.fee,
                        "fail-open: pool resolve miss"
                    );
                    keep_unmeasured.push(p.tp);
                }
            }
        }
    }

    if items.is_empty() {
        // Nada medido — devolve o que tínhamos (fail-open total).
        return Ok(keep_unmeasured);
    }

    // --- multicall balanceOf ---
    let mut multicall = Multicall::new(client.clone(), None).await?;
    for it in &items {
        let erc_a = ERC20::new(it.addr_a, client.clone());
        let erc_b = ERC20::new(it.addr_b, client.clone());
        multicall.add_call(erc_a.balance_of(it.pool), true);
        multicall.add_call(erc_b.balance_of(it.pool), true);
    }

    let raw: Vec<Result<Token, _>> = match multicall.call_raw().await {
        Ok(r) => r,
        Err(e) => {
            warn!(
                target: "liquidity",
                "liquidity multicall failed ({e}) — fail-open (mantém preços)"
            );
            let mut out: Vec<_> = items.into_iter().map(|i| i.tp).collect();
            out.extend(keep_unmeasured);
            return Ok(out);
        }
    };

    let mut kept = keep_unmeasured;
    kept.reserve(items.len());
    for (i, it) in items.into_iter().enumerate() {
        let decode = |idx: usize| -> Option<U256> {
            match raw.get(idx) {
                Some(Ok(Token::Uint(v))) => Some(*v),
                _ => None,
            }
        };
        let (Some(bal_a), Some(bal_b)) = (decode(i * 2), decode(i * 2 + 1)) else {
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", it.tp.token_a, it.tp.token_b),
                dex = %it.tp.dex_name,
                "fail-open: balanceOf decode miss"
            );
            kept.push(it.tp);
            continue;
        };

        let pa = token_price_usd(&it.tp.token_a).await;
        let pb = token_price_usd(&it.tp.token_b).await;

        // Sem preço nos dois lados → não mede; fail-open.
        if pa <= 0.0 && pb <= 0.0 {
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", it.tp.token_a, it.tp.token_b),
                dex = %it.tp.dex_name,
                "fail-open: sem price_usd para TVL"
            );
            kept.push(it.tp);
            continue;
        }

        let tvl = pool_tvl_usd_from_balances(bal_a, it.dec_a, pa, bal_b, it.dec_b, pb);
        if passes_liquidity_gate(tvl, min_usd) {
            kept.push(it.tp);
        } else {
            discarded += 1;
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", it.tp.token_a, it.tp.token_b),
                dex = %it.tp.dex_name,
                tvl_usd = tvl,
                min_usd,
                bal_a = %bal_a,
                bal_b = %bal_b,
                "descartado: TVL proxy abaixo do threshold"
            );
        }
    }

    note_low_liquidity_discarded(discarded);
    if discarded > 0 {
        info!(
            target: "liquidity",
            discarded,
            kept = kept.len(),
            min_usd,
            total_discarded = low_liquidity_discarded_count(),
            "liquidity gate batch"
        );
    }

    Ok(kept)
}

/// Lê TVL (USD) de UM pool, read-only, sem gas. Reusa a mesma lógica do gate
/// (`filter_token_prices_by_liquidity`): resolve/cacheia endereço do pool via
/// `resolve_pool`, faz `balanceOf(token_a, pool)` + `balanceOf(token_b, pool)`
/// num multicall (1 round-trip), e converte via `pool_tvl_usd_from_balances`
/// + `PRICE_FEED`. Uniforme V2/V3/Curve (balanceOf serve p/ todos; Curve
/// tipicamente resolve `None` → fail-open).
///
/// Retorna `Ok(None)` p/ qualquer falha mensurável (pool miss, token_cache miss,
/// multicall fail, sem preço, decode miss) — **fail-open**, nunca erro fatal.
/// Usado pelo log `[TOPSPREAD]` p/ revelar pool raso vs pool profundo.
pub async fn read_pool_tvl_usd<F, Fut>(
    client: Arc<AppMiddleware>,
    token_cache: &TokenCache,
    dex_name: &str,
    token_a: &str,
    token_b: &str,
    fee_hint: u32,
    mut resolve_pool: F,
) -> Result<Option<f64>>
where
    F: FnMut(String, Address, Address, u32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<Address>>>,
{
    let (Some(info_a), Some(info_b)) = (
        token_cache.get_by_symbol(token_a).await,
        token_cache.get_by_symbol(token_b).await,
    ) else {
        return Ok(None); // fail-open: token_cache miss
    };
    let dec_a = cached_decimals(info_a.address, info_a.decimals);
    let dec_b = cached_decimals(info_b.address, info_b.decimals);

    // 1) Pool address (cache ou resolve).
    let pool = if let Some(p) = cached_pool_address(dex_name, token_a, token_b, fee_hint) {
        p
    } else {
        match resolve_pool(
            dex_name.to_string(),
            info_a.address,
            info_b.address,
            fee_hint,
        )
        .await
        {
            Ok(Some(p)) if !p.is_zero() => {
                cache_pool_address(dex_name, token_a, token_b, fee_hint, p);
                p
            }
            _ => return Ok(None), // fail-open: pool resolve miss (Curve, etc.)
        }
    };

    // 2) Multicall balanceOf (2 calls, 1 round-trip).
    let mut multicall = Multicall::new(client.clone(), None).await?;
    let erc_a = ERC20::new(info_a.address, client.clone());
    let erc_b = ERC20::new(info_b.address, client.clone());
    multicall.add_call(erc_a.balance_of(pool), true);
    multicall.add_call(erc_b.balance_of(pool), true);

    let raw: Vec<Result<Token, _>> = match multicall.call_raw().await {
        Ok(r) => r,
        Err(e) => {
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", token_a, token_b),
                dex = %dex_name,
                "read_pool_tvl_usd: multicall failed ({e}) — fail-open"
            );
            return Ok(None);
        }
    };
    let (bal_a, bal_b) = match (raw.get(0), raw.get(1)) {
        (Some(Ok(Token::Uint(a))), Some(Ok(Token::Uint(b)))) => (*a, *b),
        _ => {
            debug!(
                target: "liquidity",
                pair = %format!("{}-{}", token_a, token_b),
                dex = %dex_name,
                "read_pool_tvl_usd: balanceOf decode miss — fail-open"
            );
            return Ok(None);
        }
    };

    // 3) Preços via feed (stables fallback $1). Sem preço nos dois lados → fail-open.
    let pa = token_price_usd(token_a).await;
    let pb = token_price_usd(token_b).await;
    if pa <= 0.0 && pb <= 0.0 {
        return Ok(None);
    }

    let tvl = pool_tvl_usd_from_balances(bal_a, dec_a, pa, bal_b, dec_b, pb);
    Ok(Some(tvl))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn threshold_from_config_only() {
        let mut cfg = Config::default();
        cfg.arbitrage.min_liquidity = "12345.5".into();
        assert!((min_pool_liquidity_usd(&cfg) - 12345.5).abs() < 1e-9);
        // per-dex ausente => cai no global
        assert!((min_pool_liquidity_usd_for_dex(&cfg, "QuickSwap") - 12345.5).abs() < 1e-9);
        cfg.arbitrage.min_liquidity = "not_a_number".into();
        assert!((min_pool_liquidity_usd(&cfg) - 5000.0).abs() < 1e-9);
    }

    /// `[[dex]].liquidity_threshold_usd` era descartado pelo parser — todo venue
    /// usava o global. Agora manda no venue e cai no global só se ausente.
    #[test]
    fn per_dex_threshold_overrides_global() {
        use crate::config::DexEntry;
        let mut cfg = Config::default();
        cfg.arbitrage.min_liquidity = "50000".into();
        cfg.dex = vec![
            DexEntry {
                name: "UniswapV3".into(),
                router_address: String::new(),
                liquidity_threshold_usd: Some(10_000.0),
                ..Default::default()
            },
            DexEntry {
                name: "SushiSwap".into(),
                router_address: String::new(),
                liquidity_threshold_usd: None,
                ..Default::default()
            },
        ];
        assert_eq!(min_pool_liquidity_usd_for_dex(&cfg, "UniswapV3"), 10_000.0);
        // case-insensitive
        assert_eq!(min_pool_liquidity_usd_for_dex(&cfg, "uniswapv3"), 10_000.0);
        // sem override => global
        assert_eq!(min_pool_liquidity_usd_for_dex(&cfg, "SushiSwap"), 50_000.0);
        // venue desconhecido => global
        assert_eq!(min_pool_liquidity_usd_for_dex(&cfg, "Curve"), 50_000.0);
    }

    #[test]
    fn tvl_from_balances_uses_price_feed_math() {
        // 1000 USDC (6 dec) @ $1 + 2 WETH (18 dec) @ $2000 = $1000 + $4000 = $5000
        let bal_usdc = U256::from(1_000_000_000u64); // 1000 * 1e6
        let bal_weth = U256::from(2u64) * U256::exp10(18);
        let tvl = pool_tvl_usd_from_balances(bal_usdc, 6, 1.0, bal_weth, 18, 2000.0);
        assert!((tvl - 5000.0).abs() < 1e-6, "tvl={tvl}");
    }

    #[test]
    fn gate_discards_below_keeps_above() {
        assert!(!passes_liquidity_gate(4999.0, 5000.0));
        assert!(passes_liquidity_gate(5000.0, 5000.0));
        assert!(passes_liquidity_gate(50_000.0, 5000.0));
        assert!(!passes_liquidity_gate(f64::NAN, 5000.0));
    }

    #[test]
    fn pool_cache_roundtrip_symmetric() {
        let pool = Address::from_low_u64_be(0xBEEF);
        cache_pool_address("QuickSwap", "USDC", "WETH", 3000, pool);
        assert_eq!(
            cached_pool_address("QuickSwap", "WETH", "USDC", 3000),
            Some(pool)
        );
        assert_eq!(
            cached_pool_address("QuickSwap", "WETH", "USDC", 500),
            None,
            "fee entra na chave do cache"
        );
    }

    #[test]
    fn discard_counter_increments() {
        reset_low_liquidity_discarded_count();
        note_low_liquidity_discarded(3);
        assert_eq!(low_liquidity_discarded_count(), 3);
        reset_low_liquidity_discarded_count();
        assert_eq!(low_liquidity_discarded_count(), 0);
    }
}
