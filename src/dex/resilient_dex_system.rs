// ============================================================
// src/dex/resilient_dex_system.rs — v5.1.0 (FACTORY CHECK FIX COMPLETO)
// ============================================================
//
// 🚀 CORREÇÃO CRÍTICA: Implementação de `get_pair_or_pool_address` para ResilientDex (E0046)
// ✅ Adicionado `get_wbtc_address` (delegado)
//
// ============================================================

use crate::{
    config::Config,
    dex::{DexContract, TokenPairPrice},
    AppMiddleware,
};
use anyhow::Result;
use async_trait::async_trait;
use ethers::types::{Address, U256};
use std::sync::Arc;
use tracing::{debug, instrument, warn};

#[derive(Clone)]
pub struct ResilientDex<D: DexContract + Send + Sync + 'static> {
    primary: Arc<D>,
    fallback: Option<Arc<D>>,
    config: Arc<Config>,
}

impl<D: DexContract + Send + Sync + 'static> ResilientDex<D> {
    pub fn new(primary: Arc<D>, fallback: Option<Arc<D>>, config: Arc<Config>) -> Self {
        Self {
            primary,
            fallback,
            config,
        }
    }
}

#[async_trait]
impl<D: DexContract + Send + Sync + 'static> DexContract for ResilientDex<D> {
    fn name(&self) -> String {
        format!("Resilient({})", self.primary.name())
    }

    // Funções de endereço (wrappers - delegadas ao primary)
    fn get_wmatic_address(&self) -> Option<Address> {
        self.primary.get_wmatic_address()
    }
    fn get_weth_address(&self) -> Option<Address> {
        self.primary.get_weth_address()
    }
    fn get_usdc_address(&self) -> Option<Address> {
        self.primary.get_usdc_address()
    }
    fn get_usdt_address(&self) -> Option<Address> {
        self.primary.get_usdt_address()
    }
    fn get_dai_address(&self) -> Option<Address> {
        self.primary.get_dai_address()
    }
    fn get_wbtc_address(&self) -> Option<Address> {
        self.primary.get_wbtc_address()
    } // 🆕 WBTC

    // 🚀 NOVO: Implementação de get_pair_or_pool_address (Factory Check)
    // Delega a checagem da Factory para o adapter primário.
    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>> {
        self.primary
            .get_pair_or_pool_address(token_a, token_b)
            .await
    }

    async fn get_pool_address_for_liquidity(
        &self,
        token_a: Address,
        token_b: Address,
        fee_hint: u32,
    ) -> Result<Option<Address>> {
        self.primary
            .get_pool_address_for_liquidity(token_a, token_b, fee_hint)
            .await
    }

    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        match self.primary.get_price(token_a, token_b).await {
            Ok(Some(price)) => {
                debug!("✅ [ResilientDex] Preço obtido do primary: {:.6}", price);
                return Ok(Some(price));
            }
            Ok(None) => {
                debug!("📭 [ResilientDex] Primary sem liquidez");
            }
            Err(e) => {
                warn!("⚠️ [ResilientDex] Primary falhou: {}", e);
            }
        }

        if let Some(fallback) = &self.fallback {
            match fallback.get_price(token_a, token_b).await {
                Ok(Some(price)) => {
                    debug!("🔄 [ResilientDex] Preço obtido do fallback: {:.6}", price);
                    return Ok(Some(price));
                }
                Ok(None) => {
                    debug!("📭 [ResilientDex] Fallback sem liquidez");
                }
                Err(e) => {
                    warn!("⚠️ [ResilientDex] Fallback falhou: {}", e);
                }
            }
        }

        warn!("❌ [ResilientDex] Todos os adapters falharam");
        Ok(None)
    }

    // ============================================================
    // 🔹 GET PRICES MULTICALL (delegado)
    // ============================================================
    #[instrument(skip_all, fields(dex = %self.name()))]
    async fn get_prices_multicall(
        &self,
        pairs: &[(String, String)],
        quote_block: Option<ethers::types::U64>,
    ) -> Result<Vec<TokenPairPrice>> {
        // Para multicall, tentamos o primário. Se falhar, tentamos o fallback.
        match self.primary.get_prices_multicall(pairs, quote_block).await {
            Ok(prices) if !prices.is_empty() => {
                debug!(
                    "✅ [ResilientDex] Multicall bem-sucedido no primary {}",
                    self.primary.name()
                );
                Ok(prices)
            }
            Ok(_) => {
                // Sucesso, mas sem preços (sem liquidez). Tentar fallback.
                debug!("📭 [ResilientDex] Multicall no primary {} sem resultados, tentando fallback...", self.primary.name());
                if let Some(fallback) = &self.fallback {
                    fallback.get_prices_multicall(pairs, quote_block).await
                } else {
                    Ok(vec![]) // Sem fallback, retorna vazio
                }
            }
            Err(e) => {
                warn!(
                    "⚠️ [ResilientDex] Multicall no primary {} falhou: {}. Tentando fallback...",
                    self.primary.name(),
                    e
                );
                if let Some(fallback) = &self.fallback {
                    fallback.get_prices_multicall(pairs, quote_block).await
                } else {
                    Err(e) // Sem fallback, propaga o erro
                }
            }
        }
    }

    async fn swap(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256> {
        self.primary.swap(token_in, token_out, amount_in).await
    }

    async fn get_token_address(&self, symbol: &str) -> Result<Address> {
        self.primary.get_token_address(symbol).await
    }

    async fn get_amount_with_decimals(
        &self,
        token_address: Address,
        base_amount: f64,
    ) -> Result<U256> {
        self.primary
            .get_amount_with_decimals(token_address, base_amount)
            .await
    }

    fn client(&self) -> &Arc<AppMiddleware> {
        self.primary.client()
    }

    fn config(&self) -> &Arc<Config> {
        &self.config
    }
}
