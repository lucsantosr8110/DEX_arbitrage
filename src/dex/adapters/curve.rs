// ============================================================
// src/dex/adapters/curve.rs — Curve Finance Adapter (Polygon)
// ============================================================
// Pool amDAI/amUSDC/amUSDT: fee 0.04%, TVL ~$2.3M
// ============================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    dex::{
        calculate_price_from_decimals,
        normalize_price,
        quote_amount_for_usd,
        rate_limiter::ALCHEMY_RATE_LIMITER,
        DexContract,
        TokenPairPrice,
    },
    AppMiddleware,
};

use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ethers::{
    abi::{Abi, Token},
    contract::Contract,
    types::{Address, U256},
};
use std::{str::FromStr, sync::Arc};
use tracing::{debug, info, warn};

const DEX_NAME: &str = "Curve";

// ABI mínima para Curve pool (get_dy + coins)
const CURVE_POOL_ABI: &str = r#"[
    {
        "name": "get_dy",
        "outputs": [{"type": "uint256", "name": ""}],
        "inputs": [{"type": "int128", "name": "i"}, {"type": "int128", "name": "j"}, {"type": "uint256", "name": "dx"}],
        "stateMutability": "view",
        "type": "function"
    },
    {
        "name": "coins",
        "outputs": [{"type": "address", "name": ""}],
        "inputs": [{"type": "uint256", "name": "i"}],
        "stateMutability": "view",
        "type": "function"
    },
    {
        "name": "balances",
        "outputs": [{"type": "uint256", "name": ""}],
        "inputs": [{"type": "uint256", "name": "i"}],
        "stateMutability": "view",
        "type": "function"
    }
]"#;

// Pool principal de stables na Polygon
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";

// Tokens wrapped do Aave usados no Curve
const AM_USDC: &str = "0x1a13F4Ca1d028320A707D99520AbFefca3998b7F";
const AM_USDT: &str = "0x60D55F02A771d515e077c9C2403a1ef324885CeC";
const AM_DAI: &str = "0x27F8D03b3a2196956ED754baDc28D73be8830A6e";

#[derive(Clone)]
pub struct CurveDex {
    client: Arc<AppMiddleware>,
    pool_address: Address,
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
    pool_tokens: Vec<(Address, u8, String)>, // (am_address, decimals, original_symbol)
}

impl CurveDex {
    pub async fn new(client: Arc<AppMiddleware>, pool_addr: Address, config: Arc<Config>) -> Self {
        let token_cache = TokenCache::global(config.clone()).await;

        // Ordem real do pool (verificado via API): amDAI(idx 0), amUSDC(idx 1), amUSDT(idx 2)
        let pool_tokens = vec![
            (Address::from_str(AM_DAI).unwrap(), 18, "DAI".to_string()),
            (Address::from_str(AM_USDC).unwrap(), 6, "USDC".to_string()),
            (Address::from_str(AM_USDT).unwrap(), 6, "USDT".to_string()),
        ];

        info!("✅ {}Dex inicializado | pool={} | 3 stables (0.04% fee)", DEX_NAME, pool_addr);
        Self {
            client,
            pool_address: pool_addr,
            config,
            token_cache,
            pool_tokens,
        }
    }

    fn load_abi(abi_str: &str) -> Result<Abi> {
        serde_json::from_str(abi_str).map_err(|e| anyhow!("ABI parse error: {}", e))
    }

    fn pool_index(&self, symbol: &str) -> Option<usize> {
        self.pool_tokens
            .iter()
            .position(|(_, _, orig)| orig.eq_ignore_ascii_case(symbol))
    }

    fn quote_notional_usd(&self) -> f64 {
        self.config
            .arbitrage
            .default_trade_amount
            .parse::<f64>()
            .unwrap_or(100.0)
    }

    async fn resolve_am_to_symbol(&self, addr: &Address) -> Option<String> {
        self.pool_tokens
            .iter()
            .find(|(am_addr, _, _)| am_addr == addr)
            .map(|(_, _, sym)| sym.clone())
    }

    fn symbol_from_pair(&self, pair: &str) -> Option<String> {
        // "USDC-USDT" → Some("USDC"), etc.
        let sym = pair.split('-').next()?;
        let normalized = match sym.to_uppercase().as_str() {
            "USDC" | "USDT" | "DAI" => sym.to_uppercase(),
            "MATIC" | "WMATIC" => return None, //不在 Curve pool
            _ => return None,
        };
        Some(normalized)
    }
}

#[async_trait]
impl DexContract for CurveDex {
    fn name(&self) -> String {
        DEX_NAME.to_string()
    }

    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        let sym_a = self.resolve_am_to_symbol(token_a).await;
        let sym_b = self.resolve_am_to_symbol(token_b).await;

        let (Some(idx_a), Some(idx_b)) = (
            sym_a.as_deref().and_then(|s| self.pool_index(s)),
            sym_b.as_deref().and_then(|s| self.pool_index(s)),
        ) else {
            return Ok(None);
        };

        let abi = Self::load_abi(CURVE_POOL_ABI)?;
        let pool = Contract::new(self.pool_address, abi, self.client.clone());

        let decimals_a = self.pool_tokens[idx_a].1;
        let decimals_b = self.pool_tokens[idx_b].1;

        let amount_in = quote_amount_for_usd(
            sym_a.as_deref().unwrap_or("USDC"),
            decimals_a,
            self.quote_notional_usd(),
        )
        .await
        .unwrap_or(U256::from(10u64.pow(decimals_a as u32)));

        ALCHEMY_RATE_LIMITER.acquire().await?;

        let dy: U256 = pool
            .method("get_dy", (idx_a as i128, idx_b as i128, amount_in))?
            .call()
            .await
            .map_err(|e| anyhow!("Curve get_dy failed: {}", e))?;

        if dy.is_zero() {
            return Ok(None);
        }

        let price = calculate_price_from_decimals(amount_in, dy, decimals_a, decimals_b)?;
        Ok(normalize_price(price))
    }

    async fn get_prices_multicall(&self, pairs: &[(String, String)]) -> Result<Vec<TokenPairPrice>> {
        let abi = Self::load_abi(CURVE_POOL_ABI)?;
        let pool = Contract::new(self.pool_address, abi, self.client.clone());

        let mut results = Vec::new();

        for (token_a, token_b) in pairs {
            // Só processa pares de stables
            let (Some(idx_a), Some(idx_b)) = (
                self.pool_index(token_a),
                self.pool_index(token_b),
            ) else {
                continue;
            };

            let decimals_a = self.pool_tokens[idx_a].1;
            let decimals_b = self.pool_tokens[idx_b].1;

            let amount_in = quote_amount_for_usd(token_a, decimals_a, self.quote_notional_usd())
                .await
                .unwrap_or(U256::from(10u64.pow(decimals_a as u32)));

            ALCHEMY_RATE_LIMITER.acquire().await?;

            let dy: U256 = match pool
                .method("get_dy", (idx_a as i128, idx_b as i128, amount_in))?
                .call()
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    debug!("[{}] get_dy({}→{}) failed: {}", DEX_NAME, token_a, token_b, e);
                    continue;
                }
            };

            if dy.is_zero() {
                continue;
            }

            if let Ok(price) = calculate_price_from_decimals(amount_in, dy, decimals_a, decimals_b) {
                if let Some(normalized) = normalize_price(price) {
                    results.push(
                        TokenPairPrice::new(token_a.clone(), token_b.clone(), normalized, DEX_NAME.into())
                            .with_fee_tier(4), // 0.04% = 4 bps
                    );
                }
            }
        }

        Ok(results)
    }

    async fn swap(&self, _token_in: Address, _token_out: Address, _amount_in: U256) -> Result<U256> {
        Err(anyhow!("Curve swap not implemented (read-only adapter)"))
    }

    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>> {
        let sym_a = self.resolve_am_to_symbol(&token_a).await;
        let sym_b = self.resolve_am_to_symbol(&token_b).await;

        if sym_a.is_some() && sym_b.is_some() {
            Ok(Some(self.pool_address))
        } else {
            Ok(None)
        }
    }

    fn config(&self) -> &Arc<Config> {
        &self.config
    }

    fn client(&self) -> &Arc<AppMiddleware> {
        &self.client
    }
}
