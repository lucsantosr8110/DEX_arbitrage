// ============================================================
// src/dex/adapters/quickswap.rs — v6.1.2 (FINAL FIXES)
// ============================================================
// 🚀 CORRIGIDO: E0599 - get_wmatic_address chamado via self.config()
// ============================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    dex::{
        calculate_price_from_decimals,
        get_token_decimals::get_token_decimals,
        normalize_price,
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
    contract::{Contract, Multicall},
    types::{Address, U256},
};
use std::{collections::HashSet, str::FromStr, sync::Arc};
use tracing::{debug, info, warn};

// --- ABIS E CONSTANTES ---
// ABI para consultar getPair na Factory (o método que evita reverts)
const V2_FACTORY_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenA","type":"address"},{"internalType":"address","name":"tokenB","type":"address"}],"name":"getPair","outputs":[{"internalType":"address","name":"pair","type":"address"}],"stateMutability":"view","type":"function"}]"#;

const UNISWAP_V2_ROUTER_ABI: &str = r#"[{
    "type": "function",
    "name": "getAmountsOut",
    "inputs": [
        {"name": "amountIn", "type": "uint256"},
        {"name": "path", "type": "address[]"}
    ],
    "outputs": [{"name": "", "type": "uint256[]"}],
    "stateMutability": "view"
}]"#;

const DEX_NAME: &str = "QuickSwap";

// CORREÇÃO E0412/E0422: Movido para o escopo do arquivo.
#[derive(Clone)]
struct CallInfo {
    token_a: String,
    token_b: String,
    addr_a: Address,
    addr_b: Address,
    decimals_a: u8,
    decimals_b: u8,
    amount_in: U256,
}

#[derive(Clone)]
pub struct QuickSwapDex {
    client: Arc<AppMiddleware>,
    router: Address,
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
}

impl QuickSwapDex {
    pub async fn new(client: Arc<AppMiddleware>, router: Address, config: Arc<Config>) -> Self {
        let token_cache = TokenCache::global(config.clone()).await;
        info!("✅ {}Dex inicializado com router {}", DEX_NAME, router);
        Self {
            client,
            router,
            config,
            token_cache,
        }
    }
    
    // Helper para converter string de ABI em ABI (usado para multicall e swap)
    fn load_abi_from_wrapper(abi_str: &str) -> Result<Abi> {
        serde_json::from_str(abi_str)
            .map_err(|e| anyhow!("Falha ao carregar ABI do wrapper: {}", e))
    }

    async fn resolve_token(&self, symbol_or_address: &str) -> Result<Address> {
        if symbol_or_address.starts_with("0x") {
            return Ok(Address::from_str(symbol_or_address)?);
        }
        self.token_cache
            .resolve(symbol_or_address)
            .await
            .ok_or_else(|| anyhow!("Token não suportado: {}", symbol_or_address))
    }
    
    async fn prepare_call_data(&self, pairs: &[(String, String)]) -> Result<Vec<CallInfo>> {
        let mut call_data_list = Vec::new();
        for (token_a, token_b) in pairs {
            
            let (Ok(addr_a), Ok(addr_b)) = (
                self.resolve_token(token_a).await,
                self.resolve_token(token_b).await,
            ) else {
                warn!("⚠️ [{}] Falha ao resolver par {}/{} (endereços)", DEX_NAME, token_a, token_b);
                continue;
            };

            let (Ok(decimals_a), Ok(decimals_b)) = (
                get_token_decimals(self.client.clone(), addr_a).await,
                get_token_decimals(self.client.clone(), addr_b).await,
            ) else {
                warn!("⚠️ [{}] Falha ao resolver par {}/{} (decimais)", DEX_NAME, token_a, token_b);
                continue;
            };
            
            let amount_in = U256::exp10(decimals_a as usize);
            call_data_list.push(CallInfo {
                token_a: token_a.clone(),
                token_b: token_b.clone(),
                addr_a,
                addr_b,
                decimals_a,
                decimals_b,
                amount_in,
            });
        }
        Ok(call_data_list)
    }
}

#[async_trait]
impl DexContract for QuickSwapDex {
    fn name(&self) -> String {
        DEX_NAME.into()
    }
    
    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>> {
        let factory_address_option = self
            .config()
            .dex
            .iter()
            .find(|d| d.name == self.name())
            .ok_or_else(|| anyhow!("Configuração DEX não encontrada em config.toml para {}", self.name()))?
            .factory_address 
            .clone();

        let factory_address_str = factory_address_option
            .ok_or_else(|| anyhow!("Factory address ausente na config.toml para {}", self.name()))?;

        let factory_address = factory_address_str.parse::<Address>()?;
        
        let abi: Abi = serde_json::from_str(V2_FACTORY_ABI)?;
        let factory = Contract::new(factory_address, abi, self.client.clone());

        let call = factory.method::<_, Address>("getPair", (token_a, token_b))?;

        match call.call().await {
            Ok(pair_address) => {
                if pair_address.is_zero() {
                    Ok(None)
                } else {
                    Ok(Some(pair_address))
                }
            }
            Err(e) => {
                warn!("[{}] Factory check falhou (getPair): {}", self.name(), e);
                Ok(None) 
            }
        }
    }


    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        let abi: Abi = serde_json::from_str(UNISWAP_V2_ROUTER_ABI)?;
        let contract = Contract::new(self.router, abi.clone(), self.client.clone());

        let (Ok(decimals_a), Ok(decimals_b)) = (
            get_token_decimals(self.client.clone(), *token_a).await,
            get_token_decimals(self.client.clone(), *token_b).await,
        ) else {
            warn!("⚠️ [{}] get_price: Falha ao resolver decimais", self.name());
            return Ok(None);
        };

        let amount_in = U256::exp10(decimals_a as usize);
        let path = vec![*token_a, *token_b];
        let call = contract.method::<_, Vec<U256>>("getAmountsOut", (amount_in, path))?;
        
        ALCHEMY_RATE_LIMITER.acquire().await?; 
        match call.call().await {
            Ok(amounts) => {
                if let Some(amount_out) = amounts.last() {
                    if !amount_out.is_zero() {
                        let price = calculate_price_from_decimals(
                            amount_in, *amount_out, decimals_a, decimals_b
                        )?;
                        return Ok(normalize_price(price));
                    }
                }
                Ok(None)
            }
            Err(e) => {
                debug!("- [{}] get_price falhou: {}", self.name(), e.to_string());
                Ok(None)
            }
        }
    }

    async fn get_prices_multicall(
        &self,
        pairs: &[(String, String)],
    ) -> Result<Vec<TokenPairPrice>> {
        
        let mut prices = Vec::new();
        let mut failed_pairs = HashSet::new(); 

        let abi: Abi = serde_json::from_str(UNISWAP_V2_ROUTER_ABI)?;
        let contract = Contract::new(self.router, abi.clone(), self.client.clone());

        let call_data_list = self.prepare_call_data(pairs).await?;
        if call_data_list.is_empty() {
            return Ok(prices);
        }

        let mut multicall_direct = Multicall::new(self.client.clone(), None).await?;
        
        for info in &call_data_list {
            let path = vec![info.addr_a, info.addr_b];
            let call = contract.method::<_, Vec<U256>>("getAmountsOut", (info.amount_in, path))?;
            multicall_direct.add_call(call, true);
        }

        debug!("⚡ [{}] Multicall Pass 1 (Direct) - {} chamadas", DEX_NAME, call_data_list.len());
        ALCHEMY_RATE_LIMITER.acquire().await?; 
        let results_direct: Vec<Result<Token, _>> = multicall_direct.call_raw().await?;

        for (i, result) in results_direct.into_iter().enumerate() {
            let info = &call_data_list[i];
            let pair_id = (info.token_a.clone(), info.token_b.clone());

            match result {
                Ok(Token::Array(tokens)) => {
                    if let Some(Token::Uint(amount_out)) = tokens.last() {
                         if !amount_out.is_zero() {
                            let price = calculate_price_from_decimals(
                                info.amount_in, *amount_out, info.decimals_a, info.decimals_b
                            )?;
                            if let Some(p) = normalize_price(price) {
                                prices.push(TokenPairPrice::new(info.token_a.clone(), info.token_b.clone(), p, self.name()));
                                continue; 
                            }
                        }
                    }
                }
                Ok(other) => {
                    debug!("- [{}] Path direto {}/{} retornou Token inesperado: {:?}", DEX_NAME, info.token_a, info.token_b, other);
                }
                Err(e) => {
                     debug!("- [{}] Path direto falhou para {}/{}: {}", DEX_NAME, info.token_a, info.token_b, e.to_string());
                }
            }
            failed_pairs.insert(pair_id);
        }

        // 🚀 CORREÇÃO E0599: Chamada direta ao método do trait usando `self`
        let Some(wmatic_addr) = self.get_wmatic_address() else {
             warn!("⚠️ [{}] Não foi possível obter o endereço WMATIC, pulando fallback.", DEX_NAME);
             return Ok(prices); 
        };
        
        let mut multicall_wmatic = Multicall::new(self.client.clone(), None).await?;
        let mut wmatic_call_map = Vec::new(); 

        for info in call_data_list.into_iter() {
            let pair_id = (info.token_a.clone(), info.token_b.clone());
            if failed_pairs.contains(&pair_id) && info.addr_a != wmatic_addr && info.addr_b != wmatic_addr {
                let path = vec![info.addr_a, wmatic_addr, info.addr_b];
                let call = contract.method::<_, Vec<U256>>("getAmountsOut", (info.amount_in, path))?;
                multicall_wmatic.add_call(call, true);
                wmatic_call_map.push(info); 
            }
        }

        if wmatic_call_map.is_empty() {
            return Ok(prices); 
        }

        debug!("⚡ [{}] Multicall Pass 2 (WMATIC) - {} chamadas", DEX_NAME, wmatic_call_map.len());
        ALCHEMY_RATE_LIMITER.acquire().await?; 
        let results_wmatic: Vec<Result<Token, _>> = multicall_wmatic.call_raw().await?;

        for (i, result) in results_wmatic.into_iter().enumerate() {
             let info = &wmatic_call_map[i];
             if let Ok(Token::Array(tokens)) = result {
                 if let Some(Token::Uint(amount_out)) = tokens.last() {
                     if !amount_out.is_zero() {
                        let price = calculate_price_from_decimals(
                            info.amount_in, *amount_out, info.decimals_a, info.decimals_b
                        )?;
                        if let Some(p) = normalize_price(price) {
                            info!("🔁 [{}] Fallback via WMATIC: {}/{}", DEX_NAME, info.token_a, info.token_b);
                            prices.push(TokenPairPrice::new(info.token_a.clone(), info.token_b.clone(), p, self.name()));
                        }
                     }
                 }
             }
        }
        Ok(prices)
    }

    async fn swap(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256> {
        if self.config.execution.dry_run {
            info!("[{}] Dry-run swap {:?} -> {:?} (QuickSwap)", DEX_NAME, token_in, token_out);
            return Ok(amount_in);
        }

        let abi = Self::load_abi_from_wrapper(include_str!("../../../abi/uniswap_v2_router.json"))?;
        let router = Contract::new(self.router, abi, self.client.clone());

        let params = (
            amount_in,
            U256::zero(),
            vec![token_in, token_out],
            self.client.address(),
            U256::from(chrono::Utc::now().timestamp() as u64 + 600),
        );

        let method = router.method::<_, U256>("swapExactTokensForTokens", params)?;
        ALCHEMY_RATE_LIMITER.acquire().await?;
        let pending = method.send().await?;
        if let Some(r) = pending.await? {
            info!("✅ [{}] Swap executado: {:?}", DEX_NAME, r.transaction_hash);
        }

        Ok(amount_in)
    }

    fn client(&self) -> &Arc<AppMiddleware> { &self.client }
    fn config(&self) -> &Arc<Config> { &self.config }
}