//! Warm-boot de metadata (address + decimals + pool) **fora do hot path**.
//!
//! - Exige decimals do `[pairs.metadata]` — **sem** fallback silencioso para 18
//!   (WBTC=8, USDC=6, WETH/WMATIC=18).
//! - Popula `get_token_decimals` cache e `liquidity` pool cache.
//! - Falha barulhenta (log + Err) se metadata faltar ou divergir do on-chain.

use crate::{
    config::{token_cache::TokenCache, Config},
    dex::{cache_token_decimals, get_token_decimals, liquidity, DexContract},
    AppMiddleware,
};
use anyhow::{anyhow, bail, Context, Result};
use ethers::types::Address;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::Arc;
use tracing::{info, warn};

/// Decimals esperados para tokens do universo curado (validação estrita).
pub fn expected_decimals(symbol: &str) -> Option<u8> {
    match symbol.to_ascii_uppercase().as_str() {
        "WBTC" | "BTC" => Some(8),
        "USDC" | "USDT" | "USDC.E" => Some(6),
        "WETH" | "ETH" | "WMATIC" | "MATIC" | "WPOL" | "DAI" => Some(18),
        _ => None,
    }
}

/// Lê decimals do config metadata — **sem** default 18.
pub fn decimals_from_config(cfg: &Config, symbol: &str) -> Result<u8> {
    let key = symbol.to_ascii_uppercase();
    let meta = cfg
        .pairs
        .metadata
        .get(&key)
        .or_else(|| cfg.pairs.metadata.get(symbol))
        .ok_or_else(|| anyhow!("metadata ausente para token {}", symbol))?;
    meta.decimals.ok_or_else(|| {
        anyhow!("decimals ausente em [pairs.metadata] para {} — sem fallback 18", symbol)
    })
}

pub fn address_from_config(cfg: &Config, symbol: &str) -> Result<Address> {
    let key = symbol.to_ascii_uppercase();
    let s = cfg
        .pairs
        .tokens
        .get(&key)
        .or_else(|| cfg.pairs.tokens.get(symbol))
        .ok_or_else(|| anyhow!("[pairs.tokens] ausente para {}", symbol))?;
    Address::from_str(s).with_context(|| format!("endereço inválido para {}: {}", symbol, s))
}

/// Tokens únicos referenciados por `pairs.monitor`.
pub fn monitor_symbols(cfg: &Config) -> Vec<String> {
    let mut set = HashSet::new();
    for p in &cfg.pairs.monitor {
        if let Some((a, b)) = p.split_once('-') {
            set.insert(a.trim().to_ascii_uppercase());
            set.insert(b.trim().to_ascii_uppercase());
        }
    }
    let mut v: Vec<_> = set.into_iter().collect();
    v.sort();
    v
}

/// Warm-boot: valida config ↔ on-chain, cacheia decimals e pools.
/// Deve rodar no boot (DexManager), nunca no loop do radar.
pub async fn warm_monitor_metadata(
    client: Arc<AppMiddleware>,
    cfg: &Config,
    token_cache: &TokenCache,
    adapters: &[Arc<dyn DexContract + Send + Sync>],
) -> Result<WarmReport> {
    let symbols = monitor_symbols(cfg);
    if symbols.is_empty() {
        warn!(target: "metadata_warm", "pairs.monitor vazio — warm skip");
        return Ok(WarmReport::default());
    }

    let mut report = WarmReport::default();

    for sym in &symbols {
        let addr = address_from_config(cfg, sym)?;
        let cfg_dec = decimals_from_config(cfg, sym)?;
        if let Some(exp) = expected_decimals(sym) {
            if cfg_dec != exp {
                bail!(
                    "decimals config {}={} diverge do esperado {} (sem fallback)",
                    sym,
                    cfg_dec,
                    exp
                );
            }
        }

        // On-chain — falha = abort (não assume 18).
        let onchain = get_token_decimals(client.clone(), addr)
            .await
            .with_context(|| format!("decimals on-chain falhou para {}", sym))?;
        if onchain != cfg_dec {
            bail!(
                "decimals on-chain {}={} != config {}",
                sym,
                onchain,
                cfg_dec
            );
        }
        cache_token_decimals(addr, cfg_dec);
        report.tokens_warmed += 1;
        info!(
            target: "metadata_warm",
            symbol = %sym,
            ?addr,
            decimals = cfg_dec,
            "token metadata warmed"
        );
    }

    // Pools: resolve + cache (fora do hot path).
    for p in &cfg.pairs.monitor {
        let Some((a, b)) = p.split_once('-') else {
            continue;
        };
        let a = a.trim().to_ascii_uppercase();
        let b = b.trim().to_ascii_uppercase();
        let (Some(ia), Some(ib)) = (
            token_cache.get_by_symbol(&a).await,
            token_cache.get_by_symbol(&b).await,
        ) else {
            warn!(
                target: "metadata_warm",
                pair = %p,
                "token_cache miss no warm de pool — skip"
            );
            continue;
        };

        for ad in adapters {
            let dex = ad.name();
            match ad
                .get_pool_address_for_liquidity(ia.address, ib.address, 3000)
                .await
            {
                Ok(Some(pool)) if !pool.is_zero() => {
                    // Cache nas duas fees comuns V2(3000) e V3 fee do quote depois sobrescreve.
                    liquidity::cache_pool_address(&dex, &a, &b, 3000, pool);
                    for fee in [500u32, 3000, 10_000] {
                        if let Ok(Some(pfee)) = ad
                            .get_pool_address_for_liquidity(ia.address, ib.address, fee)
                            .await
                        {
                            if !pfee.is_zero() {
                                liquidity::cache_pool_address(&dex, &a, &b, fee, pfee);
                            }
                        }
                    }
                    report.pools_warmed += 1;
                    info!(
                        target: "metadata_warm",
                        dex = %dex,
                        pair = %format!("{}-{}", a, b),
                        ?pool,
                        "pool address warmed"
                    );
                }
                Ok(_) => {
                    debug_skip(&dex, &a, &b, "pool zero/none");
                }
                Err(e) => {
                    warn!(
                        target: "metadata_warm",
                        dex = %dex,
                        pair = %format!("{}-{}", a, b),
                        error = %e,
                        "pool resolve falhou no warm"
                    );
                }
            }
        }
    }

    info!(
        target: "metadata_warm",
        tokens = report.tokens_warmed,
        pools = report.pools_warmed,
        "metadata warm-boot completo (hot path sem RPC de metadata)"
    );
    Ok(report)
}

fn debug_skip(dex: &str, a: &str, b: &str, why: &str) {
    tracing::debug!(
        target: "metadata_warm",
        dex,
        pair = %format!("{}-{}", a, b),
        why,
        "pool warm skip"
    );
}

#[derive(Debug, Default, Clone)]
pub struct WarmReport {
    pub tokens_warmed: usize,
    pub pools_warmed: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn expected_decimals_curated() {
        assert_eq!(expected_decimals("WBTC"), Some(8));
        assert_eq!(expected_decimals("USDC"), Some(6));
        assert_eq!(expected_decimals("WETH"), Some(18));
        assert_eq!(expected_decimals("WMATIC"), Some(18));
    }

    #[test]
    fn decimals_from_config_no_silent_18() {
        let mut cfg = Config::default();
        // sem metadata → Err (não 18)
        assert!(decimals_from_config(&cfg, "WBTC").is_err());

        cfg.pairs.metadata.insert(
            "WBTC".into(),
            crate::config::TokenMetadata {
                symbol: "WBTC".into(),
                decimals: Some(8),
                ..Default::default()
            },
        );
        assert_eq!(decimals_from_config(&cfg, "WBTC").unwrap(), 8);

        cfg.pairs.metadata.insert(
            "USDC".into(),
            crate::config::TokenMetadata {
                symbol: "USDC".into(),
                decimals: None,
                ..Default::default()
            },
        );
        assert!(decimals_from_config(&cfg, "USDC").is_err());
    }

    #[test]
    fn monitor_symbols_from_pairs() {
        let mut cfg = Config::default();
        cfg.pairs.monitor = vec![
            "USDC-WETH".into(),
            "WBTC-USDC".into(),
            "WMATIC-WETH".into(),
        ];
        let s = monitor_symbols(&cfg);
        assert_eq!(s, vec!["USDC", "WBTC", "WETH", "WMATIC"]);
    }
}
