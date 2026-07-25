// ============================================================
// src/dex/adapters/balancer.rs — Balancer V2 Adapter (Polygon)
// ============================================================
// Vault: 0xBA12222222228d8Ba445958a75a0704d566BF2C8
// Usa queryBatchSwap para obter preços sem executar swaps
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
    types::{Address, U256, I256},
};
use std::{str::FromStr, sync::Arc};
use tracing::{debug, info};

const DEX_NAME: &str = "Balancer";

// Balancer V2 Vault on Polygon
const BALANCER_VAULT: &str = "0xBA12222222228d8Ba445958a75a0704d566BF2C8";

// Known stablecoins on Polygon
const TOKENS: &[(&str, &str, u8)] = &[
    ("USDC", "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174", 6),
    ("USDT", "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", 6),
    ("DAI", "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063", 18),
    ("WMATIC", "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270", 18),
    ("WETH", "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619", 18),
];

#[derive(Clone)]
pub struct BalancerDex {
    client: Arc<AppMiddleware>,
    vault: Address,
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
    token_addrs: Vec<(String, Address, u8)>,
}

impl BalancerDex {
    pub async fn new(client: Arc<AppMiddleware>, config: Arc<Config>) -> Self {
        let token_cache = TokenCache::global(config.clone()).await;
        let vault = Address::from_str(BALANCER_VAULT).unwrap();

        let token_addrs: Vec<(String, Address, u8)> = TOKENS
            .iter()
            .map(|(sym, addr, dec)| {
                (
                    sym.to_string(),
                    Address::from_str(addr).unwrap(),
                    *dec,
                )
            })
            .collect();

        info!("✅ {}Dex inicializado | vault={} | {} tokens", DEX_NAME, vault, token_addrs.len());
        Self {
            client,
            vault,
            config,
            token_cache,
            token_addrs,
        }
    }

    fn load_abi() -> Result<Abi> {
        let abi_str = include_str!("../../../abi/BalancerVault.json");
        serde_json::from_str(abi_str).map_err(|e| anyhow!("Balancer ABI parse error: {}", e))
    }

    fn get_token_info(&self, symbol: &str) -> Option<(&str, Address, u8)> {
        self.token_addrs.iter().find(|(sym, _, _)| sym == symbol).map(|(s, a, d)| (s.as_str(), *a, *d))
    }

    fn get_token_info_by_addr(&self, addr: &Address) -> Option<(&str, Address, u8)> {
        self.token_addrs.iter().find(|(_, a, _)| a == addr).map(|(s, a, d)| (s.as_str(), *a, *d))
    }

    fn quote_notional_usd(&self) -> f64 {
        self.config
            .arbitrage
            .default_trade_amount
            .parse::<f64>()
            .unwrap_or(100.0)
    }
}

#[async_trait]
impl DexContract for BalancerDex {
    fn name(&self) -> String {
        DEX_NAME.to_string()
    }

    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        let info_a = self.get_token_info_by_addr(token_a);
        let info_b = self.get_token_info_by_addr(token_b);

        let (Some((sym_a, addr_a, dec_a)), Some((_, addr_b, dec_b))) = (info_a, info_b) else {
            return Ok(None);
        };

        let amount_in = quote_amount_for_usd(sym_a, dec_a, self.quote_notional_usd())
            .await
            .unwrap_or(U256::from(10u64.pow(dec_a as u32)));

        ALCHEMY_RATE_LIMITER.acquire().await?;

        let abi = Self::load_abi()?;
        let vault = Contract::new(self.vault, abi, self.client.clone());

        let swap = Token::Tuple(vec![
            Token::Address(addr_a),
            Token::Address(addr_b),
            Token::Uint(amount_in),
        ]);

        let result: Vec<I256> = vault
            .method("queryBatchSwap", (0u8, vec![swap], self.vault))?
            .call()
            .await
            .map_err(|e| anyhow!("Balancer queryBatchSwap failed: {}", e))?;

        if result.is_empty() || result[0].is_negative() || result[0].is_zero() {
            return Ok(None);
        }

        let amount_out = result[0].into_raw();
        let price = calculate_price_from_decimals(amount_in, amount_out, dec_a, dec_b)?;
        Ok(normalize_price(price))
    }

    async fn get_prices_multicall(&self, pairs: &[(String, String)]) -> Result<Vec<TokenPairPrice>> {
        let mut results = Vec::new();
        let abi = Self::load_abi()?;

        info!("[{}] Querying {} pairs", DEX_NAME, pairs.len());

        for (token_a, token_b) in pairs {
            let info_a = self.get_token_info(token_a);
            let info_b = self.get_token_info(token_b);

            let (Some((sym_a, addr_a, dec_a)), Some((sym_b, addr_b, dec_b))) = (info_a, info_b) else {
                continue;
            };

            let amount_in = quote_amount_for_usd(token_a, dec_a, self.quote_notional_usd())
                .await
                .unwrap_or(U256::from(10u64.pow(dec_a as u32)));

            ALCHEMY_RATE_LIMITER.acquire().await?;

            let vault = Contract::new(self.vault, abi.clone(), self.client.clone());

            let swap = Token::Tuple(vec![
                Token::Address(addr_a),
                Token::Address(addr_b),
                Token::Uint(amount_in),
            ]);

            let result: Vec<I256> = match vault
                .method("queryBatchSwap", (0u8, vec![swap], self.vault))?
                .call()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    info!("[{}] queryBatchSwap({}→{}) failed: {}", DEX_NAME, token_a, token_b, e);
                    continue;
                }
            };

            info!("[{}] queryBatchSwap({}→{}) result: {:?}", DEX_NAME, token_a, token_b, result);

            if result.is_empty() || result[0].is_negative() || result[0].is_zero() {
                continue;
            }

            let amount_out = result[0].into_raw();

            if let Ok(price) = calculate_price_from_decimals(amount_in, amount_out, dec_a, dec_b) {
                if let Some(normalized) = normalize_price(price) {
                    results.push(
                        TokenPairPrice::new(sym_a.to_string(), sym_b.to_string(), normalized, DEX_NAME.into())
                            .with_fee_tier(25),
                    );
                }
            }
        }

        Ok(results)
    }

    async fn swap(&self, _token_in: Address, _token_out: Address, _amount_in: U256) -> Result<U256> {
        Err(anyhow!("Balancer swap not implemented (read-only adapter)"))
    }

    async fn get_pair_or_pool_address(&self, token_a: Address, token_b: Address) -> Result<Option<Address>> {
        let info_a = self.get_token_info_by_addr(&token_a);
        let info_b = self.get_token_info_by_addr(&token_b);

        if info_a.is_some() && info_b.is_some() {
            Ok(Some(self.vault))
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
