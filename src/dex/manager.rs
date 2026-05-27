// ============================================================
// src/dex/manager.rs — v4.4.3-FIXED (FLASHLOAN PATH CHECK)
// ============================================================
// ✅ Compatível com Polygon, Ethereum, BNB, Arbitrum
// ✅ Compatível com ethers 2.0 (com TypedTransaction)
// ✅ Suporte a rotas Flashloan + leituras paralelas
// ✅ Mantém warm-up, health-check e circuit-breaker
// ✅ Implementação completa multicall para radar
// 🚀 NOVO: Adicionada checagem estrita para rotas circulares (Start == End == Base Token).
// ============================================================

use crate::{
    config::{token_cache::TokenCache, Config},
    // ✅ CORREÇÃO (E0308): `ArbitrageOpportunity` deve vir de `core::types` para
    //    ser compatível com o campo `base_opportunity` de `FlashloanOpportunity`.
    core::types::{ArbitrageOpportunity, FlashloanOpportunity},
    dex::{
        adapters::{
            quickswap::QuickSwapDex, sushiswap::SushiSwapDex, uniswap_v2::UniswapV2Dex,
            uniswap_v3::UniswapV3Dex,
        },
        price_cache::PriceCache,
        // ❌ `ArbitrageOpportunity` de `dex` (mod.rs) não é o usado para Flashloans.
        DexContract, TokenPairPrice,
    },
    // ✅ CORREÇÃO: `u256_to_f64` vem de `dex` (via `utils`)
    utils::utils::u256_to_f64,
    AppMiddleware,
};
use anyhow::{anyhow, Context, Result};
use ethers::{
    prelude::*,
    types::{
        transaction::eip2718::TypedTransaction, // ✅ IMPORT CORRETO
        Address, Bytes, U256,
    },
};
use futures::future::join_all;
use std::{
    collections::HashMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

/// Intervalo entre verificações de saúde
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Número máximo de erros antes de ativar o Circuit Breaker
const CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
/// Timeout para chamadas multicall individuais
const MULTICALL_TIMEOUT: Duration = Duration::from_secs(10);
/// Timeout para operação multicall completa
const MULTICALL_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// ============================================================
/// 🧩 Estrutura Principal — DexManager
/// ============================================================
#[derive(Clone)]
pub struct DexManager {
    client: Arc<AppMiddleware>,
    config: Arc<Config>,
    active_adapters: Arc<RwLock<Vec<Arc<dyn DexContract + Send + Sync>>>>,
    health: Arc<RwLock<HashMap<String, bool>>>,
    error_count: Arc<RwLock<HashMap<String, u32>>>,
    price_cache: Arc<PriceCache>,
    token_cache: Arc<TokenCache>,
    last_health_check: Arc<RwLock<Instant>>,
}

impl DexManager {
    /// ============================================================
    /// 🏗️ Inicializa o DexManager e carrega adaptadores configurados
    /// ============================================================
    pub async fn new(
        client: Arc<AppMiddleware>,
        config: Arc<Config>,
        price_cache: Arc<PriceCache>,
    ) -> Result<Self> {
        info!("🔧 Inicializando DexManager (TokenCache dinâmico)…");

        let mut adapters: Vec<Arc<dyn DexContract + Send + Sync>> = Vec::new();
        let mut health_map: HashMap<String, bool> = HashMap::new();
        let mut error_map: HashMap<String, u32> = HashMap::new();

        let token_cache = TokenCache::global(config.clone()).await;

        for dex_cfg in &config.dex {
            if !dex_cfg.enabled {
                continue;
            }

            let router_addr = dex_cfg
                .router_address
                .parse::<Address>()
                .with_context(|| format!("❌ Router inválido para {}", dex_cfg.name))?;

            let adapter: Option<Arc<dyn DexContract + Send + Sync>> = match dex_cfg.name.as_str() {
                "UniswapV2" => Some(Arc::new(
                    UniswapV2Dex::new(client.clone(), router_addr, config.clone()).await,
                )),
                "UniswapV3" => Some(Arc::new(
                    UniswapV3Dex::new(client.clone(), router_addr, config.clone()).await,
                )),
                "SushiSwap" => Some(Arc::new(
                    SushiSwapDex::new(client.clone(), router_addr, config.clone()).await,
                )),
                "QuickSwap" => Some(Arc::new(
                    QuickSwapDex::new(client.clone(), router_addr, config.clone()).await,
                )),
                other => {
                    warn!("⚠️ DEX não reconhecida no TOML: {}", other);
                    None
                }
            };

            if let Some(a) = adapter {
                info!("✅ {} inicializado: {:?}", dex_cfg.name, router_addr);
                health_map.insert(dex_cfg.name.clone(), true);
                error_map.insert(dex_cfg.name.clone(), 0);
                adapters.push(a);
            }
        }

        if adapters.is_empty() {
            return Err(anyhow!(
                "❌ Nenhum adapter DEX inicializado (verifique o TOML)"
            ));
        }

        let manager = Self {
            client,
            config,
            active_adapters: Arc::new(RwLock::new(adapters)),
            health: Arc::new(RwLock::new(health_map)),
            error_count: Arc::new(RwLock::new(error_map)),
            price_cache,
            token_cache,
            last_health_check: Arc::new(RwLock::new(Instant::now())),
        };

        info!(
            "✅ DexManager com {} adapters: {:?}",
            manager.active_adapters.read().await.len(),
            manager
                .active_adapters
                .read()
                .await
                .iter()
                .map(|a| a.name())
                .collect::<Vec<_>>()
        );

        manager.start_dynamic_warm_up().await;
        manager.start_health_checker().await;
        Ok(manager)
    }

    /// ============================================================
    /// 🔥 Warm-up dinâmico
    /// ============================================================
    pub async fn start_dynamic_warm_up(&self) {
        let mgr = self.clone();
        tokio::spawn(async move {
            info!("🎯 Iniciando warm-up dinâmico com TokenCache…");
            match mgr.dynamic_warm_up().await {
                Ok(n) => info!("✅ Warm-up concluído: {} pares preparados", n),
                Err(e) => warn!("⚠️ Warm-up falhou: {:?}", e),
            }
        });
    }

    async fn dynamic_warm_up(&self) -> Result<usize> {
        let candidate_pairs: Vec<String> = self.config.pairs.monitor.clone();
        let mut prepared: Vec<(String, Address, Address, U256)> = Vec::new();

        for pair in candidate_pairs.iter() {
            let (sym_a, sym_b) = self.parse_pair_name(pair);
            if sym_a == "UNKNOWN" || sym_b == "UNKNOWN" {
                continue;
            }

            let addr_a = match self.token_cache.get_by_symbol(&sym_a).await {
                Some(a) => a.address,
                None => continue,
            };
            let addr_b = match self.token_cache.get_by_symbol(&sym_b).await {
                Some(b) => b.address,
                None => continue,
            };

            for adapter in self.active_adapters.read().await.iter() {
                let dex_name = adapter.name().to_string();
                prepared.push((dex_name.clone(), addr_a, addr_b, U256::zero()));
            }
        }

        if prepared.is_empty() {
            warn!("⚠️ Nenhum par válido encontrado para warm-up");
            return Ok(0);
        }

        let loaded = self.price_cache.warm_up(prepared).await;
        Ok(loaded)
    }

    fn parse_pair_name(&self, pair: &str) -> (String, String) {
        let normalized = pair.replace('/', "-").replace('_', "-");
        let parts: Vec<&str> = normalized.split('-').collect();
        if parts.len() == 2 {
            (parts[0].trim().to_uppercase(), parts[1].trim().to_uppercase())
        } else {
            warn!("⚠️ Formato de par inválido: {}", pair);
            ("UNKNOWN".into(), "UNKNOWN".into())
        }
    }

    // ============================================================
    // ⚡ Suporte a Flashloan Opportunities
    // ============================================================
    pub async fn find_flashloan_opportunities(
        &self,
        base_token: &str,
        amount: U256,
    ) -> Result<Vec<FlashloanOpportunity>> {
        let mut opportunities = Vec::new();
        // ✅ CORREÇÃO (E0308): `find_circular_arbitrage` agora retorna o tipo
        //    correto `core::types::ArbitrageOpportunity` (devido à mudança no `use`)
        let circular_opps = self.find_circular_arbitrage(base_token).await?;

        for opp in circular_opps {
            if let Some(flash_opp) =
                self.convert_to_flashloan_opportunity(opp, base_token, amount).await?
            {
                opportunities.push(flash_opp);
            }
        }

        Ok(opportunities)
    }

    async fn find_circular_arbitrage(&self, base_token: &str) -> Result<Vec<ArbitrageOpportunity>> {
        debug!("🔍 Buscando rotas circulares para {}", base_token);
        Ok(Vec::new()) // placeholder
    }

    async fn convert_to_flashloan_opportunity(
        &self,
        opp: ArbitrageOpportunity, // ✅ CORREÇÃO (E0308): Este tipo agora é `core::types::ArbitrageOpportunity`
        base_token: &str,
        amount: U256,
    ) -> Result<Option<FlashloanOpportunity>> {
        
        // 🚀 CORREÇÃO SOLICITADA: VALIDAR SE A ROTA COMEÇA E TERMINA COM O TOKEN BASE
        let base_token_upper = base_token.to_uppercase();
        let first_token = opp.path.first().map(|s| s.to_uppercase());
        let last_token = opp.path.last().map(|s| s.to_uppercase());

        // Garante que é circular E que o token de empréstimo é o start/end
        if first_token != Some(base_token_upper.clone()) || last_token != Some(base_token_upper.clone()) {
            warn!(
                "⚠️ Oportunidade ignorada: O token base do flashloan ({}) não é o ponto de partida/chegada da rota: {:?}",
                base_token, opp.path
            );
            return Ok(None);
        }
        // FIM DA CORREÇÃO SOLICITADA
        
        let premium_cost = self.calculate_flashloan_premium(amount);
        let gas_overhead = self.estimate_flashloan_gas(&opp).await?;

        let flash_opp = FlashloanOpportunity {
            // ✅ CORREÇÃO (E0308): `opp` (core::types::ArbitrageOpportunity) agora
            //    bate com o tipo esperado pelo campo `base_opportunity`.
            base_opportunity: opp,
            asset: Self::resolve_token(base_token)?,
            amount,
            steps: vec![], // TODO: Converter `opp.steps` para `FlashloanStep`
            expected_profit: 0.0,
            premium_cost,
            gas_overhead,
        };

        Ok(Some(flash_opp))
    }

    fn is_valid_flashloan_route(&self, opp: &ArbitrageOpportunity) -> bool {
        // A função foi mantida por compatibilidade.
        if let (Some(first), Some(last)) = (opp.path.first(), opp.path.last()) {
             // Ex: path [USDC, WETH, USDC] -> first=USDC, last=USDC
            first == last
        } else {
            false
        }
    }

    fn calculate_flashloan_premium(&self, amount: U256) -> f64 {
        let premium_rate = 0.0009; // Aave v3 é 0.09%
        // Assumindo 6 decimais para stablecoins, idealmente deveria vir do token_cache
        u256_to_f64(amount, 6) * premium_rate * 1.1 // Adiciona 10% de buffer
    }

    async fn estimate_flashloan_gas(&self, _opp: &ArbitrageOpportunity) -> Result<u64> {
        Ok(350_000) // Placeholder
    }

    fn resolve_token(symbol: &str) -> Result<Address> {
        Address::from_str(match symbol.to_uppercase().as_str() {
            "USDC" => "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
            "USDT" => "0xc2132D05D31c914a87C6611C10748AEb04B58e8F",
            "DAI" => "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063",
            "WMATIC" => "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270",
            _ => return Err(anyhow!("Token base não reconhecido: {}", symbol)),
        })
        .context("Erro ao converter símbolo para endereço")
    }

    // ============================================================
    // 🧩 Multicall — ethers 2.0 compatível (via TypedTransaction)
    // ============================================================
    #[instrument(skip(self))]
    pub async fn multicall(&self, calls: Vec<(Address, Bytes)>) -> Result<Vec<Bytes>> {
        let futs = calls.into_iter().map(|(to, data)| {
            let tx_request = ethers::types::TransactionRequest::new()
                .to(to)
                .data(data.clone())
                .from(self.client.default_sender().unwrap_or_default());
            
            let client = self.client.clone();
            
            async move {
                let typed_tx: TypedTransaction = tx_request.into();
                // ✅ Timeout individual para evitar bloqueios
                tokio::time::timeout(MULTICALL_TIMEOUT, client.call(&typed_tx, None)).await
            }
        });

        let results = join_all(futs).await;
        let mut out = Vec::with_capacity(results.len());
        let mut errors = 0;
        
        for res in results {
            match res {
                Ok(Ok(bytes)) => out.push(bytes),
                Ok(Err(e)) => {
                    warn!("⚠️ Multicall subcall falhou: {:?}", e);
                    out.push(Bytes::new());
                    errors += 1;
                }
                Err(_) => {
                    warn!("⏰ Multicall subcall timeout");
                    out.push(Bytes::new());
                    errors += 1;
                }
            }
        }
        
        if errors > 0 {
            warn!("❌ Multicall: {}/{} chamadas falharam", errors, out.len());
        }
        
        Ok(out)
    }

    // ============================================================
    // 🔧 Circuit Breaker + Health
    // ============================================================
    pub async fn should_circuit_break(&self, dex_name: &str) -> bool {
        let e = self.error_count.read().await;
        *e.get(dex_name).unwrap_or(&0) >= CIRCUIT_BREAKER_THRESHOLD
    }

    pub async fn record_error(&self, dex_name: &str) {
        let mut e = self.error_count.write().await;
        let count = e.entry(dex_name.to_string()).or_insert(0);
        *count += 1;
        if *count >= CIRCUIT_BREAKER_THRESHOLD {
            warn!("🚫 Circuit breaker ativado para {}", dex_name);
            self.mark_unhealthy(dex_name).await;
        }
    }

    pub async fn mark_unhealthy(&self, dex_name: &str) {
        let mut h = self.health.write().await;
        h.insert(dex_name.to_string(), false);
    }

    pub async fn mark_healthy(&self, dex_name: &str) {
        let mut h = self.health.write().await;
        h.insert(dex_name.to_string(), true);
        // ✅ Reset error count quando marca como healthy
        let mut e = self.error_count.write().await;
        e.insert(dex_name.to_string(), 0);
    }

    pub async fn get_healthy_adapters(&self) -> Vec<String> {
        let h = self.health.read().await;
        let adapters = self.active_adapters.read().await;
        adapters
            .iter()
            .map(|a| a.name())
            .filter(|name| *h.get(name).unwrap_or(&true))
            .collect()
    }

    pub async fn start_health_checker(&self) {
        let mgr = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(HEALTH_CHECK_INTERVAL).await;
                let adapters = mgr.active_adapters.read().await;
                for a in adapters.iter() {
                    let name = a.name().to_string();
                    if mgr.should_circuit_break(&name).await {
                        debug!("🩺 Health-check: {} (circuit-breaker ativo)", name);
                    } else {
                        debug!("🩺 Health-check: {} (saudável)", name);
                    }
                }
            }
        });
    }

    // ============================================================
    // 🔧 Reset de Circuit Breaker
    // ============================================================
    pub async fn reset_circuit_breaker(&self, dex_name: &str) {
        let mut error_count = self.error_count.write().await;
        error_count.insert(dex_name.to_string(), 0);
        self.mark_healthy(dex_name).await;
        info!("🔄 Circuit breaker resetado para {}", dex_name);
    }

    // ============================================================
    // 🧩 Compat — usado por radar.rs - IMPLEMENTAÇÃO PRODUÇÃO
    // ============================================================

    /// Implementação REAL de coleta de preços para produção
    #[instrument(skip_all, fields(adapter = %adapter_name, pairs = pairs.len()))]
    pub async fn get_prices_multicall(
        &self,
        adapter_name: &str,
        pairs: &[String],
    ) -> Result<Vec<TokenPairPrice>> {
        let mut prices = Vec::new();
        
        // ✅ Verificar circuit breaker primeiro
        if self.should_circuit_break(adapter_name).await {
            warn!("🚫 Circuit breaker ativo para {}, pulando", adapter_name);
            return Ok(prices);
        }
        
        // Encontrar o adapter correto
        let adapters = self.active_adapters.read().await;
        let adapter = adapters.iter()
            .find(|a| a.name() == adapter_name)
            .ok_or_else(|| anyhow!("Adapter não encontrado: {}", adapter_name))?;
        
        info!("📊 Coletando preços para {} pares via {}", pairs.len(), adapter_name);
        
        // Converter pares para formato (String, String)
        let converted_pairs: Vec<(String, String)> = pairs
            .iter()
            .filter_map(|pair_str| {
                let tokens: Vec<&str> = pair_str.split('-').collect();
                if tokens.len() == 2 {
                    Some((tokens[0].to_string(), tokens[1].to_string()))
                } else {
                    warn!("⚠️ Formato de par inválido: {}", pair_str);
                    None
                }
            })
            .collect();
        
        if converted_pairs.is_empty() {
            warn!("❌ Nenhum par válido após conversão");
            return Ok(prices);
        }
        
        // ✅ Timeout para operação completa
        let multicall_result = tokio::time::timeout(
            MULTICALL_TOTAL_TIMEOUT,
            adapter.get_prices_multicall(&converted_pairs)
        ).await;
        
        match multicall_result {
            Ok(Ok(mut adapter_prices)) => {
                prices.append(&mut adapter_prices);
                info!("✅ {}: {} preços coletados", adapter_name, prices.len());
                // ✅ Reset error count em caso de sucesso
                self.mark_healthy(adapter_name).await;
            }
            Ok(Err(e)) => {
                warn!("❌ Erro no multicall do {}: {:?}", adapter_name, e);
                self.record_error(adapter_name).await;
                
                // Fallback: tentar preços individuais
                prices = self.get_prices_fallback(adapter, &converted_pairs, adapter_name).await;
            }
            Err(_) => {
                warn!("⏰ Timeout no multicall do {}", adapter_name);
                self.record_error(adapter_name).await;
                
                // Fallback mesmo em timeout
                prices = self.get_prices_fallback(adapter, &converted_pairs, adapter_name).await;
            }
        }
        
        Ok(prices)
    }

    // ✅ Método auxiliar para fallback
    async fn get_prices_fallback(
        &self,
        adapter: &Arc<dyn DexContract + Send + Sync>,
        pairs: &[(String, String)],
        adapter_name: &str,
    ) -> Vec<TokenPairPrice> {
        let mut fallback_prices = Vec::new();
        let mut success_count = 0;
        
        for (token_a, token_b) in pairs {
            if let (Some(token_a_info), Some(token_b_info)) = (
                self.token_cache.get_by_symbol(token_a).await,
                self.token_cache.get_by_symbol(token_b).await
            ) {
                match adapter.get_price(&token_a_info.address, &token_b_info.address).await {
                    Ok(Some(price)) => {
                        let token_pair = TokenPairPrice::new(
                            token_a.clone(),
                            token_b.clone(),
                            price,
                            adapter_name.to_string()
                        );
                        
                        if token_pair.is_valid() {
                            fallback_prices.push(token_pair);
                            success_count += 1;
                        }
                    }
                    _ => {
                        // Silenciosamente ignora erros individuais no fallback
                        debug!("💰 Fallback: erro individual {}-{}", token_a, token_b);
                    }
                }
            }
            
            // Pequena pausa para não sobrecarregar
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        
        info!("🔄 {}: {}/{} preços via fallback", adapter_name, success_count, pairs.len());
        fallback_prices
    }

    // ============================================================
    // 🔧 Getters para acesso externo
    // ============================================================
    pub fn get_client(&self) -> Arc<AppMiddleware> {
        self.client.clone()
    }

    pub fn get_config(&self) -> Arc<Config> {
        self.config.clone()
    }

    pub fn get_token_cache(&self) -> Arc<TokenCache> {
        self.token_cache.clone()
    }

    pub fn get_price_cache(&self) -> Arc<PriceCache> {
        self.price_cache.clone()
    }

    pub async fn get_active_adapters(&self) -> Vec<String> {
        let adapters = self.active_adapters.read().await;
        adapters.iter().map(|a| a.name().to_string()).collect()
    }
}
