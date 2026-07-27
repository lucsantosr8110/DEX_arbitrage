// ============================================================
// src/dex/adapters/uniswap_v2.rs — v6.1.2 (FINAL FIXES)
// ============================================================
//
// 🚀 CORRIGIDO: E0599 - get_wmatic_address chamado via self.config()
//
// ============================================================

use crate::{
    config::{Config, token_cache::TokenCache},
    dex::{
        calculate_price_from_decimals,
        get_token_decimals::get_token_decimals,
        normalize_price,
        quote_amount_for_usd,
        rate_limiter::ALCHEMY_RATE_LIMITER,
        DexContract, TokenPairPrice,
    },
    AppMiddleware,
};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use ethers::{
    abi::{Abi, Token},
    contract::{Contract, Multicall}, 
    types::{Address, U64, U256},
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
/// Adapter único para todos os routers Uniswap V2-compatible.
/// QuickSwap, SushiSwap e UniswapV2 só diferem por nome/endereço no TOML.
pub struct V2Dex {
    client: Arc<AppMiddleware>,
    router: Address,
    dex_name: String,
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
}

impl V2Dex {
    pub async fn new(
        client: Arc<AppMiddleware>,
        router: Address,
        config: Arc<Config>,
        dex_name: impl Into<String>,
    ) -> Self {
        let token_cache = TokenCache::global(config.clone()).await;
        let dex_name = dex_name.into();
        info!("✅ {}Dex inicializado com router {}", dex_name, router);
        Self {
            client,
            router,
            dex_name,
            config,
            token_cache,
        }
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
                warn!("⚠️ [{}] Falha ao resolver par {}/{} (endereços)", self.name(), token_a, token_b);
                continue;
            };

            let (Ok(decimals_a), Ok(decimals_b)) = (
                get_token_decimals(self.client.clone(), addr_a).await,
                get_token_decimals(self.client.clone(), addr_b).await,
            ) else {
                warn!("⚠️ [{}] Falha ao resolver par {}/{} (decimais)", self.name(), token_a, token_b);
                continue;
            };
            
            // Cotacao dimensionada pelo notional configurado, nao por "1 unidade
            // do token de entrada" (ver dex::quote_amount_for_usd).
            let amount_in =
                quote_amount_for_usd(token_a, decimals_a, self.quote_notional_usd()).await?;
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

    #[inline]
    fn quote_notional_usd(&self) -> f64 {
        self.config.executable_trade_notional_usd()
    }
}

#[async_trait]
impl DexContract for V2Dex {
    fn name(&self) -> String {
        self.dex_name.clone()
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

        // C1: dimensionar amount_in pelo notional configurado (igual ao path
        // multicall) em vez de 1 unidade — spread medido no tamanho real de
        // execução, consistente entre get_price e get_prices_multicall.
        let symbol_a = self
            .token_cache
            .get_by_address(token_a)
            .await
            .map(|i| i.symbol)
            .unwrap_or_default();
        if symbol_a.is_empty() {
            debug!("[{}] get_price: symbol não resolvido p/ {:?}", self.name(), token_a);
            return Ok(None);
        }
        let amount_in = match quote_amount_for_usd(&symbol_a, decimals_a, self.quote_notional_usd()).await {
            Ok(amount) => amount,
            Err(e) => {
                debug!("[{}] get_price: notional indisponível: {}", self.name(), e);
                return Ok(None);
            }
        };
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
        quote_block: Option<U64>,
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
        if let Some(block) = quote_block {
            multicall_direct = multicall_direct.block(block);
        }
        
        for info in &call_data_list {
            let path = vec![info.addr_a, info.addr_b];
            let call = contract.method::<_, Vec<U256>>("getAmountsOut", (info.amount_in, path))?;
            // Pool ausente/revertido não deve descartar cotações válidas do lote.
            multicall_direct.add_call(call, true);
        }

        debug!("⚡ [{}] Multicall Pass 1 (Direct) - {} chamadas", self.name(), call_data_list.len());
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
                    debug!("- [{}] Path direto {}/{} retornou Token inesperado: {:?}", self.name(), info.token_a, info.token_b, other);
                }
                Err(e) => {
                     debug!("- [{}] Path direto falhou para {}/{}: {}", self.name(), info.token_a, info.token_b, e.to_string());
                }
            }
            failed_pairs.insert(pair_id);
        }

        // NOTA: existia aqui um segundo passe que, para todo par sem pool direto,
        // recotava pelo caminho A -> WMATIC -> B e gravava o resultado como se fosse
        // o preço spot de A-B. Esse número embute 2x fee e 2x price impact, e o
        // engine então o comparava contra o preço DIRETO de outro DEX — spread
        // fabricado, exatamente como o inverso sintético que havia no radar.
        //
        // Sem pool direto, o par simplesmente não entra no mapa. Compor rotas
        // multi-hop é responsabilidade do ArbitrageEngine (build_price_graph em
        // core/arbitrage.rs), que tem contexto para precificar cada hop. O adapter
        // não tem, e por isso não deve tentar.
        if !failed_pairs.is_empty() {
            debug!(
                "🔍 [{}] {} par(es) sem pool direto, fora do mapa (amostra: {:?})",
                self.name(),
                failed_pairs.len(),
                failed_pairs.iter().take(5).collect::<Vec<_>>()
            );
        }

        Ok(prices)
    }

    async fn swap(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256> {
        // Mantém contrato histórico: UniswapV2 é quote-only; Quick/Sushi podem
        // enviar quando dry-run está desligado.
        if self.config.execution.dry_run || self.dex_name == "UniswapV2" {
            warn!("💱 [{}] Swap simulado (modo leitura)", self.name());
            return Ok(amount_in);
        }
        let abi: Abi = serde_json::from_str(include_str!("../../../abi/uniswap_v2_router.json"))?;
        let router = Contract::new(self.router, abi, self.client.clone());
        let deadline = U256::from(chrono::Utc::now().timestamp().saturating_add(600) as u64);
        let call = router.method::<_, Vec<U256>>(
            "swapExactTokensForTokens",
            (amount_in, U256::zero(), vec![token_in, token_out], self.client.address(), deadline),
        )?;
        ALCHEMY_RATE_LIMITER.acquire().await?;
        let pending = call.send().await?;
        if let Some(receipt) = pending.await? {
            info!("✅ [{}] Swap confirmado: {:?}", self.name(), receipt.transaction_hash);
        }
        Ok(amount_in)
    }
    fn client(&self) -> &Arc<AppMiddleware> { &self.client }
    fn config(&self) -> &Arc<Config> { &self.config }
}

/// Compatibilidade de API para callers que ainda importam o nome antigo.
pub type UniswapV2Dex = V2Dex;
