// ============================================================
// src/dex/adapters/uniswap_v3.rs — V5 (FIXED: Validação de Preço Multi-Amount)
// ============================================================
//
// 🚀 CORREÇÃO CRÍTICA: Implementação de validação de preço com 
//    múltiplos amounts_in (e.g., $10, $100, $500) para mitigar slippage/liquidez concentrada.
// ✅ Substitui a chamada de preço baseada em 1 token.
//
// ============================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    dex::{
        calculate_price_from_decimals,
        get_token_decimals::get_token_decimals,
        normalize_price,
        quote_amount_for_usd,
        rate_limiter::ALCHEMY_RATE_LIMITER,
        DexContract,
        TokenPairPrice,
        addresses, // Importando endereços de Factory/Quoter
        cache_fee_tier,
        select_executable_v3_best_out,
        is_executable_v3_fee_tier,
        EXECUTABLE_V3_FEE_TIERS,
        QUOTE_V3_FEE_TIERS,
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
use std::{str::FromStr, sync::Arc};
use tracing::{debug, info, warn};

// ============================================================
// 🔧 Constantes e ABIs
// ============================================================
const DEX_NAME: &str = "UniswapV3";
// Cotação: todos os tiers (incl. 100 p/ métrica). Seleção executável: ver
// `select_executable_v3_best_out` / `EXECUTABLE_V3_FEE_TIERS` em dex/mod.rs.
const FEE_TIERS: [u32; 4] = QUOTE_V3_FEE_TIERS;
const PRICE_DEVIATION_LIMIT: f64 = 0.20; // pares voláteis: 20% desvio máximo
const STABLE_PRICE_DEVIATION_LIMIT: f64 = 0.02; // stable-stable: 2%

#[inline]
fn is_usd_stable(symbol: &str) -> bool {
    matches!(symbol.to_ascii_uppercase().as_str(), "USDC" | "USDC.E" | "USDT" | "DAI")
}

#[inline]
fn price_deviation_limit(token_a: &str, token_b: &str) -> f64 {
    if is_usd_stable(token_a) && is_usd_stable(token_b) {
        STABLE_PRICE_DEVIATION_LIMIT
    } else {
        PRICE_DEVIATION_LIMIT
    }
}

// Endereços (usando addresses do mod.rs)
const DEFAULT_QUOTER_V1: &str = addresses::UNISWAP_V3_QUOTER;
const FACTORY_ADDR: &str = addresses::UNISWAP_V3_FACTORY;
// Fallback Quoter V2 (mantido como constante interna, se não estiver no mod.rs)
const FALLBACK_QUOTER_V2: &str = "0x61fFE014bA17989E743c5F6cB21bF9697530B21e"; 

// 🚀 NOVO: ABI MÍNIMA PARA A FACTORY V3 (getPool)
const UNISWAP_V3_FACTORY_ABI: &str = r#"[
    {"type":"function","name":"getPool","inputs":[{"name":"tokenA","type":"address"},{"name":"tokenB","type":"address"},{"name":"fee","type":"uint24"}],"outputs":[{"name":"pool","type":"address"}],"stateMutability":"view"}
]"#;

// ============================================================
// 🧩 Estruturas
// ============================================================
#[derive(Clone)]
pub struct UniswapV3Dex {
    client: Arc<AppMiddleware>,
    quoter_v1: Address,
    quoter_v2: Address,
    router: Address,
    factory: Address, // Endereço da Factory V3 adicionado
    config: Arc<Config>,
    token_cache: Arc<TokenCache>,
    debug_mode: bool,
    disable_multicall: bool,
}

struct CallInfo {
    token_a: String,
    token_b: String,
    addr_a: Address,
    addr_b: Address,
    decimals_a: u8,
    decimals_b: u8,
    amount_in: U256,
}

// ============================================================
// 🔧 Implementação
// ============================================================
impl UniswapV3Dex {
    pub async fn new(client: Arc<AppMiddleware>, _router: Address, config: Arc<Config>) -> Self {
        let dex_cfg = config
            .dex
            .iter()
            .find(|d| d.name == DEX_NAME)
            .expect("❌ Configuração de UniswapV3 ausente no config.toml");

        let router = dex_cfg
            .router_address
            .parse::<Address>()
            .expect("Router inválido no config.toml");

        // Endereços dos quoters
        let quoter_v1 = Address::from_str(DEFAULT_QUOTER_V1).unwrap_or_else(|_| panic!("Endereço Quoter V1 inválido"));
        let quoter_v2 = Address::from_str(FALLBACK_QUOTER_V2).unwrap_or_else(|_| panic!("Endereço Quoter V2 inválido"));
        
        // 1. OBTENDO O ENDEREÇO DA FACTORY
        let factory = dex_cfg
            .extra
            .get("factory_address")
            .and_then(|v| v.as_str())
            .unwrap_or(FACTORY_ADDR)
            .parse::<Address>()
            .expect("Endereço de Factory UniswapV3 inválido");


        // Flags de modo
        let debug_mode = dex_cfg
            .extra
            .get("debug_mode")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let disable_multicall = dex_cfg
            .extra
            .get("disable_multicall")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let token_cache = TokenCache::global(config.clone()).await;

        info!(
            "✅ [{}] Dex inicializado | router={} | factory={} | quoter_v1={} | debug_mode={}",
            DEX_NAME, router, factory, quoter_v1, debug_mode
        );

        Self {
            client,
            quoter_v1,
            quoter_v2,
            router,
            factory, // Adicionado
            config,
            token_cache,
            debug_mode,
            disable_multicall,
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

    async fn prepare_call_data(&self, pairs: &[(&str, &str)]) -> Result<Vec<CallInfo>> {
        let mut data = Vec::new();

        // NOTE: prepare_call_data AINDA USA 1 UNIDADE COMO BASE para o Multicall fallback,
        // mas a lógica principal (get_price) irá usar a validação de múltiplos amounts.
        for (a, b) in pairs {
            let (Ok(addr_a), Ok(addr_b)) = (self.resolve_token(a).await, self.resolve_token(b).await) else {
                warn!("[{}] Falha ao resolver {}/{}", DEX_NAME, a, b);
                continue;
            };

            let (Ok(dec_a), Ok(dec_b)) = (
                get_token_decimals(self.client.clone(), addr_a).await,
                get_token_decimals(self.client.clone(), addr_b).await,
            ) else {
                warn!("[{}] Falha ao obter decimais {}/{}", DEX_NAME, a, b);
                continue;
            };

            data.push(CallInfo {
                token_a: a.to_string(),
                token_b: b.to_string(),
                addr_a,
                addr_b,
                decimals_a: dec_a,
                decimals_b: dec_b,
                amount_in: quote_amount_for_usd(a, dec_a, self.quote_notional_usd()).await?,
            });
        }

        Ok(data)
    }

    fn load_abi_from_wrapper(abi_content: &str) -> Result<Abi> {
        let wrapper: serde_json::Value =
            serde_json::from_str(abi_content).map_err(|e| anyhow!("Erro parse ABI wrapper: {e}"))?;
        let array = wrapper
            .get("abi")
            .ok_or_else(|| anyhow!("Campo 'abi' ausente"))?
            .clone();
        serde_json::from_value(array).map_err(|e| anyhow!("Erro parse array ABI: {e}"))
    }

    fn deadline(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 600
    }

    /// Obtém o amount_out cotado usando V1 ou V2 Quoter como fallback.
    async fn get_quote_with_fallback(
        &self,
        token_in: Address,
        token_out: Address,
        fee: u32,
        amount_in: U256,
    ) -> Result<U256> {
        // Tenta V1
        let abi_v1 = Self::load_abi_from_wrapper(include_str!("../../../abi/uniswap_v3_quoter.json"))?;
        let quoter_v1 = Contract::new(self.quoter_v1, abi_v1.clone(), self.client.clone());

        ALCHEMY_RATE_LIMITER.acquire().await?;
        let call_v1 = quoter_v1.method::<_, U256>(
            "quoteExactInputSingle",
            (token_in, token_out, fee, amount_in, U256::zero()),
        )?;

        match call_v1.call().await {
            Ok(amount_out) if amount_out > U256::zero() => {
                return Ok(amount_out);
            }
            // Err(e) => warn!("[{}][V1] Falhou: {}", DEX_NAME, e), // Sem log de WARN aqui para evitar poluição
            _ => (),
        }

        // Fallback V2 (caso V1 reverta)
        let abi_v2 = Self::load_abi_from_wrapper(include_str!("../../../abi/uniswap_v3_quoter_v2.json"))?;
        let quoter_v2 = Contract::new(self.quoter_v2, abi_v2, self.client.clone());

        ALCHEMY_RATE_LIMITER.acquire().await?;
        let call_v2 = quoter_v2.method::<_, (U256, U256, u32, U256)>(
            "quoteExactInputSingle",
            ((token_in, token_out, fee, amount_in, U256::zero()),),
        )?;

        match call_v2.call().await {
            Ok((out, _, _, _)) if out > U256::zero() => {
                Ok(out)
            }
            Err(e) => Err(anyhow!("Ambos quoters falharam para (in: {:?}, fee: {}): {}", amount_in, fee, e)),
            _ => Err(anyhow!("quoteExactInputSingle retornou zero")),
        }
    }
    
    // ============================================================
    // NOVO: Função para obter cotação e preço
    // ============================================================

    /// Obtém o preço de Token A em termos de Token B para um dado amount_in e fee.
    async fn get_quote_price_for_amount(
        &self,
        token_a: Address,
        token_b: Address,
        fee: u32,
        amount_in: U256,
        dec_a: u8,
        dec_b: u8,
    ) -> Result<Option<f64>> {
        if amount_in.is_zero() {
            return Ok(None);
        }

        match self.get_quote_with_fallback(token_a, token_b, fee, amount_in).await {
            Ok(amount_out) => {
                let price = calculate_price_from_decimals(amount_in, amount_out, dec_a, dec_b)?;
                Ok(normalize_price(price))
            }
            Err(e) => {
                if self.debug_mode {
                    debug!("[{}] Falha ao cotar fee {} (in:{:?}): {}", DEX_NAME, fee, amount_in, e);
                }
                Err(e)
            }
        }
    }

    /// Valida preço testando com múltiplos amounts_in
    /// Retorna o preço mais confiável (mediana de 3 testes)
    async fn validate_price_with_multiple_amounts(
        &self,
        token_a: Address,
        token_b: Address,
        best_fee: u32,
        dec_a: u8,
        dec_b: u8,
    ) -> Result<Option<f64>> {
        // C1/A5: dimensionar notionais via price_feed (igual V2). Antes era
        // 1/10/50 unidades hardcoded — para WBTC (8 dec) = $64k/$640k/$3.2M,
        // para SHIB = $0.0005. Notionais absurdos geravam price-impact errado
        // → falsos negativos em alt-coins, falsos positivos em majors.
        let symbol_a = self
            .token_cache
            .get_by_address(&token_a)
            .await
            .map(|i| i.symbol)
            .unwrap_or_default();
        let symbol_b = self
            .token_cache
            .get_by_address(&token_b)
            .await
            .map(|i| i.symbol)
            .unwrap_or_default();
        let notional = self.quote_notional_usd();
        let fallback_amt = U256::exp10(dec_a as usize);
        // ~10%, 100%, 200% do notional configurado (default $100 → $10/$100/$200).
        let a_small = if symbol_a.is_empty() {
            fallback_amt
        } else {
            quote_amount_for_usd(&symbol_a, dec_a, notional * 0.1)
                .await
                .unwrap_or(fallback_amt)
        };
        let a_mid = if symbol_a.is_empty() {
            fallback_amt
        } else {
            quote_amount_for_usd(&symbol_a, dec_a, notional)
                .await
                .unwrap_or(fallback_amt)
        };
        let a_big = if symbol_a.is_empty() {
            fallback_amt
        } else {
            quote_amount_for_usd(&symbol_a, dec_a, notional * 2.0)
                .await
                .unwrap_or(fallback_amt)
        };
        let amounts_to_test: [U256; 3] = [a_small, a_mid, a_big];

        let mut prices = Vec::new();

        for amount_in in amounts_to_test.iter() {
            if let Ok(Some(price)) = self.get_quote_price_for_amount(token_a, token_b, best_fee, *amount_in, dec_a, dec_b).await {
                prices.push(price);
            }
        }

        // Exige os 3 quotes p/ mediana robusta (A4: com 2, prices[len/2]=prices[1]
        // é o maior, não mediana — viés p/ cima).
        if prices.len() < 3 {
            warn!(
                "[{}] Pool {}/{} (fee {}) - Não foi possível cotar preço confiável em 3 amounts ({}/3)",
                DEX_NAME, token_a, token_b, best_fee, prices.len()
            );
            return Ok(None);
        }

        // 4. Calcula a mediana dos preços (3 elementos → prices[1] é mediana)
        prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median_price = prices[prices.len() / 2];

        // 5. Verifica a divergência (slippage/concentração de liquidez)
        let max_deviation = prices.iter().map(|&price| {
            ((price - median_price) / median_price).abs()
        }).fold(0.0, f64::max);

        let deviation_limit = price_deviation_limit(&symbol_a, &symbol_b);
        if max_deviation > deviation_limit {
            warn!(
                 "[{}] Pool {}/{} (fee {}) divergência ALTA ({:.2}% > {:.2}%) — Preço rejeitado.",
                 DEX_NAME, token_a, token_b, best_fee, max_deviation * 100.0, deviation_limit * 100.0
            );
            return Ok(None);
        }

        Ok(Some(median_price))
    }

}

// ============================================================
// 💡 Implementação do Trait DexContract
// ============================================================
#[async_trait]
impl DexContract for UniswapV3Dex {
    fn name(&self) -> String {
        DEX_NAME.into()
    }

    /// Checa a existência do pool na Factory para qualquer Fee Tier
    async fn get_pair_or_pool_address(
        &self,
        token_a: Address,
        token_b: Address,
    ) -> Result<Option<Address>> {
        let abi: Abi = serde_json::from_str(UNISWAP_V3_FACTORY_ABI)
            .map_err(|e| anyhow!("Falha ao parsear ABI da Factory V3: {}", e))?;
        let factory_contract = Contract::new(self.factory, abi, self.client.clone());

        for &fee in &FEE_TIERS {
            ALCHEMY_RATE_LIMITER.acquire().await?;
            let call = factory_contract.method::<_, Address>("getPool", (token_a, token_b, fee))?;
            
            match call.call().await {
                Ok(pool_addr) if !pool_addr.is_zero() => {
                    debug!("- [{}] Pool encontrado com fee {}: {}", DEX_NAME, fee, pool_addr);
                    return Ok(Some(pool_addr));
                },
                _ => {}
            }
        }

        debug!("- [{}] Nenhum Pool encontrado na Factory V3 para {:?} / {:?}", DEX_NAME, token_a, token_b);
        Ok(None)
    }

    /// Gate de liquidez: pool do fee cotado (executável), não o primeiro fee-100 raso.
    async fn get_pool_address_for_liquidity(
        &self,
        token_a: Address,
        token_b: Address,
        fee_hint: u32,
    ) -> Result<Option<Address>> {
        let abi: Abi = serde_json::from_str(UNISWAP_V3_FACTORY_ABI)
            .map_err(|e| anyhow!("Falha ao parsear ABI da Factory V3: {}", e))?;
        let factory_contract = Contract::new(self.factory, abi, self.client.clone());

        let mut fees: Vec<u32> = Vec::with_capacity(EXECUTABLE_V3_FEE_TIERS.len() + 1);
        if is_executable_v3_fee_tier(fee_hint) {
            fees.push(fee_hint);
        }
        for &f in &EXECUTABLE_V3_FEE_TIERS {
            if !fees.contains(&f) {
                fees.push(f);
            }
        }

        for fee in fees {
            // Soft-limit: não abortar o gate se o limiter estiver saturado.
            let _ = ALCHEMY_RATE_LIMITER.acquire().await;
            let call = factory_contract.method::<_, Address>("getPool", (token_a, token_b, fee))?;
            match call.call().await {
                Ok(pool_addr) if !pool_addr.is_zero() => {
                    debug!(
                        "- [{}] liquidity pool fee={} => {}",
                        DEX_NAME, fee, pool_addr
                    );
                    return Ok(Some(pool_addr));
                }
                _ => {}
            }
        }
        // Fallback: qualquer pool (incl. fee 100) — melhor que falhar o TVL.
        self.get_pair_or_pool_address(token_a, token_b).await
    }

    // ========================================================
    // 🔹 Consulta de preço (get_price)
    // ========================================================
    async fn get_price(&self, token_a: &Address, token_b: &Address) -> Result<Option<f64>> {
        // 1. CRÍTICO: Checar se existe algum pool para este par
        if self.get_pair_or_pool_address(*token_a, *token_b).await?.is_none() {
            debug!("- [{}] Nenhum Pool V3 encontrado para o par, pulando cotação.", DEX_NAME);
            return Ok(None);
        }

        let (Ok(dec_a), Ok(dec_b)) = (
            get_token_decimals(self.client.clone(), *token_a).await,
            get_token_decimals(self.client.clone(), *token_b).await,
        ) else {
            warn!("[{}] Falha ao obter decimais", DEX_NAME);
            return Ok(None);
        };

        // C1: dimensionar amount_in_test pelo notional configurado (igual ao
        // multicall) em vez de 1 unidade — tier que vence em 1 unidade pode
        // não ser o melhor no notional real (V3 concentrada muda ranking c/ size).
        let symbol_a_test = self
            .token_cache
            .get_by_address(token_a)
            .await
            .map(|i| i.symbol)
            .unwrap_or_default();
        let amount_in_test = if symbol_a_test.is_empty() {
            U256::exp10(dec_a as usize)
        } else {
            quote_amount_for_usd(&symbol_a_test, dec_a, self.quote_notional_usd())
                .await
                .unwrap_or_else(|_| U256::exp10(dec_a as usize))
        };
        let mut quotes: Vec<(u32, U256)> = Vec::with_capacity(FEE_TIERS.len());

        for &fee in &FEE_TIERS {
            if let Ok(out) = self
                .get_quote_with_fallback(*token_a, *token_b, fee, amount_in_test)
                .await
            {
                if !out.is_zero() {
                    quotes.push((fee, out));
                }
            }
        }

        let Some((best_fee, _)) = select_executable_v3_best_out(&quotes) else {
            debug!(
                "- [{}] Nenhum FEE Tier EXECUTÁVEL cotou para o par — descartado",
                DEX_NAME
            );
            return Ok(None);
        };

        // 2. NOVO: Valida o preço usando múltiplos amounts e a melhor fee encontrada
        let validated_price = self.validate_price_with_multiple_amounts(
            *token_a, 
            *token_b, 
            best_fee, 
            dec_a, 
            dec_b
        ).await?;

        // 📊 LOG DE COTAÇÃO (fim do cálculo)
        if let Some(price) = validated_price {
            // Cacheia fee tier EXECUTÁVEL (nunca 100) com chave canônica.
            if let (Some(info_a), Some(info_b)) = (
                self.token_cache.get_by_address(token_a).await,
                self.token_cache.get_by_address(token_b).await,
            ) {
                cache_fee_tier(DEX_NAME, &info_a.symbol, &info_b.symbol, best_fee);
            }

            if self.debug_mode {
                tracing::info!(
                    "[{}] {}→{} fee={} price={:.8} (validated, cached fee_tier={})",
                    DEX_NAME, token_a, token_b, best_fee, price, best_fee
                );
            }
            Ok(Some(price))
        } else {
            // Preço rejeitado devido à alta divergência ou poucos quotes
            Ok(None)
        }
    }

    // ========================================================
    // ⚡ Consulta múltipla (Multicall opcional)
    // ========================================================
    // O Multicall é mantido inalterado, pois é otimizado para velocidade 
    // e confia no revert do Quoter. A lógica de preço principal (get_price)
    // agora é mais robusta para fallback.
    async fn get_prices_multicall(&self, pairs: &[(String, String)]) -> Result<Vec<TokenPairPrice>> {
        if self.disable_multicall {
            warn!("[{}] Multicall desativado — fallback para chamadas diretas", DEX_NAME);
            let mut results = Vec::new();
            for (a, b) in pairs {
                if let (Ok(addr_a), Ok(addr_b)) = (
                    self.resolve_token(a).await,
                    self.resolve_token(b).await,
                ) {
                    if let Ok(Some(p)) = self.get_price(&addr_a, &addr_b).await {
                        results.push(TokenPairPrice::new(a.clone(), b.clone(), p, DEX_NAME.into()));
                    }
                }
            }
            return Ok(results);
        }

        let mut results = Vec::new();
        let abi = Self::load_abi_from_wrapper(include_str!("../../../abi/uniswap_v3_quoter.json"))?;
        let quoter = Contract::new(self.quoter_v1, abi, self.client.clone());
        let call_data = self
            .prepare_call_data(&pairs.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect::<Vec<_>>())
            .await?;

        // Chunk em batches de 10 pares para caber no gas limit da Alchemy
        const MULTICALL_BATCH_SIZE: usize = 10;

        for batch in call_data.chunks(MULTICALL_BATCH_SIZE) {
            let mut multicall = Multicall::new(self.client.clone(), None).await?;
            for info in batch {
                for &fee in &FEE_TIERS {
                    let call = quoter.method::<_, U256>(
                        "quoteExactInputSingle",
                        (info.addr_a, info.addr_b, fee, info.amount_in, U256::zero()),
                    )?;
                    // M16: um tier/pool inexistente pode reverter no Quoter.
                    // Em ethers Multicall >= v2, `true` permite a falha
                    // individual; `call_raw` a devolve como Err sem descartar
                    // os demais quotes do batch.
                    multicall.add_call(call, true);
                }
            }

            ALCHEMY_RATE_LIMITER.acquire().await?;
            let raw: Vec<Result<Token, _>> = match multicall.call_raw().await {
                Ok(r) => r,
                Err(e) => {
                    warn!("[{}] Multicall batch falhou ({} pares): {:?}", DEX_NAME, batch.len(), e);
                    continue;
                }
            };

            for (i, chunk) in raw.chunks_exact(FEE_TIERS.len()).enumerate() {
                let info = &batch[i];
                let mut quotes: Vec<(u32, U256)> = Vec::with_capacity(FEE_TIERS.len());

                for (j, r) in chunk.iter().enumerate() {
                    if let Ok(Token::Uint(v)) = r {
                        if !v.is_zero() {
                            quotes.push((FEE_TIERS[j], *v));
                        }
                    }
                }

                let Some((best_fee, best_out)) = select_executable_v3_best_out(&quotes) else {
                    continue;
                };

                // A3: multicall (hot path) não rodava validate_price_with_
                // multiple_amounts. Reusar os quotes JÁ fetchados dos tiers
                // executáveis p/ medir dispersão cross-tier (zero RPC extra).
                // Pools V3 concentradas/rasas mostram desvio alto entre tiers
                // — sinal de liquidez ruim. Descarta igual ao path get_price.
                let mut tier_prices: Vec<f64> = Vec::new();
                for (fee, out) in quotes.iter() {
                    if !is_executable_v3_fee_tier(*fee) || out.is_zero() {
                        continue;
                    }
                    if let Ok(p) =
                        calculate_price_from_decimals(info.amount_in, *out, info.decimals_a, info.decimals_b)
                    {
                        if p > 0.0 {
                            tier_prices.push(p);
                        }
                    }
                }
                if tier_prices.len() >= 2 {
                    tier_prices.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                    let mid = tier_prices[tier_prices.len() / 2];
                    let max_dev = tier_prices
                        .iter()
                        .map(|p| ((p - mid) / mid).abs())
                        .fold(0.0_f64, f64::max);
                    let deviation_limit = price_deviation_limit(&info.token_a, &info.token_b);
                    if max_dev > deviation_limit {
                        debug!(
                            "[{}] multicall {}/{} — dispersão cross-tier {:.2}% > {}% (pool concentrada/rasa), descartado",
                            DEX_NAME, info.token_a, info.token_b, max_dev * 100.0, deviation_limit * 100.0
                        );
                        continue;
                    }
                }

                let price = calculate_price_from_decimals(
                    info.amount_in,
                    best_out,
                    info.decimals_a,
                    info.decimals_b,
                )?;
                if self.debug_mode {
                    tracing::info!(
                        "[{}] {}→{} fee={} in={} out={} price={:.8} (multicall)",
                        DEX_NAME, info.token_a, info.token_b, best_fee, info.amount_in, best_out, price
                    );
                }
                if let Some(p) = normalize_price(price) {
                    cache_fee_tier(DEX_NAME, &info.token_a, &info.token_b, best_fee);

                    results.push(TokenPairPrice::new(
                        info.token_a.clone(),
                        info.token_b.clone(),
                        p,
                        DEX_NAME.into(),
                    ).with_fee_tier(best_fee));
                }
            }
        }

        Ok(results)
    }

    // ========================================================
    // 🔁 Execução de swap
    // ========================================================
    async fn swap(&self, token_in: Address, token_out: Address, amount_in: U256) -> Result<U256> {
        if self.config.execution.dry_run {
            info!("[{}] Dry-run swap {:?} -> {:?}", DEX_NAME, token_in, token_out);
            return Ok(amount_in);
        }
        
        // 1. CRÍTICO: Checar se o par existe antes de tentar o swap (evitar revert)
        if self.get_pair_or_pool_address(token_in, token_out).await?.is_none() {
            return Err(anyhow!("[{}] Swap: Nenhum Pool V3 encontrado para o par.", DEX_NAME));
        }

        let router_abi = Self::load_abi_from_wrapper(include_str!("../../../abi/UniswapV3Router.json"))?;
        let router = Contract::new(self.router, router_abi, self.client.clone());

        // A2: fee tier hardcoded 3000 revertia em pools fee=500/10000. Resolver
        // via cache (preenchido pelo get_price/multicall) — fallback 3000 só se
        // sem cache (e loga, pois é arriscado).
        let (sym_in, sym_out) = (
            self.token_cache.get_by_address(&token_in).await.map(|i| i.symbol),
            self.token_cache.get_by_address(&token_out).await.map(|i| i.symbol),
        );
        let fee_tier = match (sym_in.as_deref(), sym_out.as_deref()) {
            (Some(a), Some(b)) => crate::dex::cached_fee_tier(DEX_NAME, a, b),
            _ => None,
        };
        let fee = fee_tier.unwrap_or_else(|| {
            warn!(
                "[{}] swap: fee_tier não cacheado p/ {:?}/{:?} — fallback 3000 (arriscado p/ pool 500/10000)",
                DEX_NAME, token_in, token_out
            );
            3000
        });

        let params = (
            token_in,
            token_out,
            fee,
            self.client.address(),
            self.deadline(),
            amount_in,
            U256::zero(),
            U256::zero(),
        );

        let method = router.method::<_, U256>("exactInputSingle", params)?;
        ALCHEMY_RATE_LIMITER.acquire().await?;
        let pending = method.send().await?;
        if let Some(r) = pending.await? {
            info!("✅ [{}] Swap executado: {:?}", DEX_NAME, r.transaction_hash);
        }

        Ok(amount_in) 
    }

    // ========================================================
    // 🔧 Acessores
    // ========================================================
    fn client(&self) -> &Arc<AppMiddleware> {
        &self.client
    }

    fn config(&self) -> &Arc<Config> {
        &self.config
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stable_pair_uses_tighter_price_deviation_limit() {
        assert_eq!(price_deviation_limit("USDC", "USDT"), 0.02);
        assert_eq!(price_deviation_limit("DAI", "USDC"), 0.02);
        assert_eq!(price_deviation_limit("WETH", "USDC"), 0.20);
    }
}
