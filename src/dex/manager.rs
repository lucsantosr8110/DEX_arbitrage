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
            curve::CurveDex, uniswap_v2::V2Dex, uniswap_v3::UniswapV3Dex,
        },
        // ❌ `ArbitrageOpportunity` de `dex` (mod.rs) não é o usado para Flashloans.
        DexContract, TokenPairPrice,
    },
    infra::metrics,
    AppMiddleware,
};
use anyhow::{anyhow, Context, Result};
use ethers::{providers::Middleware, types::{Address, U64, U256}};
use std::{
    collections::HashMap,
    str::FromStr,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, RwLock};
use tracing::{debug, error, info, instrument, warn};

/// Intervalo entre verificações de saúde
const HEALTH_CHECK_INTERVAL: Duration = Duration::from_secs(60);
/// Número máximo de erros antes de ativar o Circuit Breaker
const CIRCUIT_BREAKER_THRESHOLD: u32 = 5;
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
                "UniswapV2" | "SushiSwap" | "QuickSwap" => Some(Arc::new(
                    V2Dex::new(client.clone(), router_addr, config.clone(), dex_cfg.name.clone()).await,
                )),
                "UniswapV3" => Some(Arc::new(
                    UniswapV3Dex::new(client.clone(), router_addr, config.clone()).await,
                )),
                "Curve" => Some(Arc::new(
                    CurveDex::new(client.clone(), router_addr, config.clone()).await,
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

        manager.start_metadata_warm().await;
        // start_health_checker() é chamado de main.rs com shutdown_rx, para
        // que o loop de health-check saia limpo no shutdown (antes era
        // tokio::spawn detached sem sinal — leaked até o processo morrer).
        Ok(manager)
    }

    /// Warm-boot address+decimals+pool (fora do hot path). Sem fallback 18.
    pub async fn start_metadata_warm(&self) {
        let client = self.client.clone();
        let cfg = self.config.clone();
        let token_cache = self.token_cache.clone();
        let adapters = self.active_adapters.read().await.clone();
        match crate::dex::metadata_warm::warm_monitor_metadata(
            client,
            cfg.as_ref(),
            token_cache.as_ref(),
            &adapters,
        )
        .await
        {
            Ok(r) => info!(
                "✅ Metadata warm-boot: {} tokens, {} pools",
                r.tokens_warmed, r.pools_warmed
            ),
            Err(e) => {
                // Barulhento: não seguir cego com decimals errados.
                error!("❌ Metadata warm-boot FALHOU: {:#}", e);
            }
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
        // Display/estimate only — execution gates use fixed_usd + on-chain premium.
        let premium_bps = 9u64; // 0.09% Aave-ish placeholder + buffer handled below
        let price = crate::core::fixed_usd::UsdE8(crate::core::fixed_usd::USD_E8_SCALE);
        let principal = crate::core::fixed_usd::token_raw_to_usd_e8(amount, price, 6)
            .unwrap_or(crate::core::fixed_usd::UsdE8::zero());
        let fee = crate::core::fixed_usd::flashloan_fee_usd_e8(principal, premium_bps)
            .unwrap_or(crate::core::fixed_usd::UsdE8::zero());
        // +10% buffer (legacy behaviour)
        fee.display_f64() * 1.1
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

    // NOTA: havia aqui um `multicall()` que disparava N `eth_call` paralelos via
    // `join_all`. Nunca foi chamado por ninguém — a agregação real acontece nos
    // adapters, que usam `ethers::contract::Multicall` (Multicall3). Removido para
    // não voltar a parecer que existe um segundo caminho de coleta de preços.

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

    pub async fn start_health_checker(&self, mut shutdown_rx: broadcast::Receiver<()>) {
        let mgr = self.clone();
        tokio::spawn(async move {
            loop {
                // Sleep com select em shutdown: antes era sleep pelado, loop
                // nunca saía no shutdown — task leaked. Agora broadcast quebra.
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("🩺 Health-check: shutdown recebido, encerrando.");
                        break;
                    }
                    _ = tokio::time::sleep(HEALTH_CHECK_INTERVAL) => {}
                }
                // Auto-recovery: DEXs marcados unhealthy não são mais consultados
                // pelo radar (get_healthy_adapters filtra health=false), então nunca
                // recebem um multicall de sucesso que chamaria mark_healthy —
                // deadlock permanente (ex.: SushiSwap após rate-limit Infura, TUI
                // fica com coluna em branco até restart). A cada ciclo, damos uma
                // nova chance: resetamos error_count/health para que o próximo scan
                // tente de novo. Se a falha persistir, record_error re-tripa o
                // breaker no mesmo ciclo.
                let unhealthy: Vec<String> = {
                    let h = mgr.health.read().await;
                    let adapters = mgr.active_adapters.read().await;
                    adapters
                        .iter()
                        .map(|a| a.name())
                        .filter(|n| matches!(h.get(n), Some(false)))
                        .map(|n| n.to_string())
                        .collect()
                };
                for name in &unhealthy {
                    info!(
                        "♻️ Health-check: resetando {} (auto-recovery — estava unhealthy, nova tentativa no próximo scan)",
                        name
                    );
                    mgr.reset_circuit_breaker(name).await;
                }
                let adapters = mgr.active_adapters.read().await;
                for a in adapters.iter() {
                    let name = a.name().to_string();
                    if unhealthy.iter().any(|u| u == &name) {
                        // já tratado acima
                        continue;
                    }
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
        quote_block: Option<U64>,
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
        
        debug!("📊 Coletando preços para {} pares via {}", pairs.len(), adapter_name);
        
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
        
        // Métrica única no manager cobre Curve/V2/V3 e também os fallbacks.
        let quote_started = Instant::now();
        metrics::inc_dex_request(adapter_name);
        // ✅ Timeout para operação completa
        let multicall_result = tokio::time::timeout(
            MULTICALL_TOTAL_TIMEOUT,
            adapter.get_prices_multicall(&converted_pairs, quote_block)
        ).await;
        
        match multicall_result {
            Ok(Ok(mut adapter_prices)) => {
                prices.append(&mut adapter_prices);
                metrics::observe_dex_quote(adapter_name, "ok", quote_started.elapsed().as_secs_f64() * 1_000.0);
                debug!("✅ {}: {} preços coletados", adapter_name, prices.len());
                // ✅ Reset error count em caso de sucesso
                self.mark_healthy(adapter_name).await;
            }
            Ok(Err(e)) => {
                metrics::observe_dex_quote(adapter_name, "error", quote_started.elapsed().as_secs_f64() * 1_000.0);
                warn!("❌ Erro no multicall do {}: {:?}", adapter_name, e);
                self.record_error(adapter_name).await;
                
                // Fallback: tentar preços individuais
                prices = self.get_prices_fallback(adapter, &converted_pairs, adapter_name).await;
            }
            Err(_) => {
                metrics::observe_dex_quote(adapter_name, "timeout", quote_started.elapsed().as_secs_f64() * 1_000.0);
                warn!("⏰ Timeout no multicall do {}", adapter_name);
                self.record_error(adapter_name).await;
                
                // Fallback mesmo em timeout
                prices = self.get_prices_fallback(adapter, &converted_pairs, adapter_name).await;
            }
        }
        
        Ok(prices)
    }

    /// Snapshot do bloco para todas as cotações de um scan.
    pub async fn quote_block_number(&self) -> Result<U64> {
        self.client
            .get_block_number()
            .await
            .context("falha ao obter bloco para snapshot de cotações")
    }

    /// Referência ao config (threshold de liquidez, etc.).
    pub fn config_ref(&self) -> &Config {
        self.config.as_ref()
    }

    /// Gate de liquidez: TVL proxy via `balanceOf(pool)` multicall.
    /// Descarta pares abaixo de `min_usd` (`arbitrage.min_liquidity`).
    pub async fn filter_prices_by_liquidity(
        &self,
        adapter_name: &str,
        prices: Vec<crate::dex::TokenPairPrice>,
        min_usd: f64,
    ) -> Result<Vec<crate::dex::TokenPairPrice>> {
        let adapters = self.active_adapters.read().await;
        let adapter = adapters
            .iter()
            .find(|a| a.name() == adapter_name)
            .cloned();
        drop(adapters);

        let Some(adapter) = adapter else {
            return Ok(prices);
        };

        let client = self.client.clone();
        let token_cache = self.token_cache.clone();
        let ad = adapter.clone();
        let ad_liq = adapter.clone();

        crate::dex::liquidity::filter_token_prices_by_liquidity(
            client,
            &token_cache,
            prices,
            min_usd,
            move |_dex, a, b, fee| {
                let ad = ad.clone();
                async move { ad.get_pool_address_for_liquidity(a, b, fee).await }
            },
            move |_dex, a, b| {
                let ad = ad_liq.clone();
                async move { ad.liquidity_token_addresses(a, b).await }
            },
        )
        .await
    }

    /// Lê TVL (USD) de UM pool, read-only, sem gas. Wrapper de
    /// `liquidity::read_pool_tvl_usd` com o resolver do adapter
    /// (`get_pool_address_for_liquidity`). `Ok(None)` = fail-open (Curve, pool
    /// miss, sem preço). Usado pelo log `[TOPSPREAD]` p/ revelar pool raso.
    pub async fn pool_tvl_usd(
        &self,
        dex_name: &str,
        token_a: &str,
        token_b: &str,
        fee_hint: u32,
    ) -> Result<Option<f64>> {
        let adapters = self.active_adapters.read().await;
        let adapter = adapters.iter().find(|a| a.name() == dex_name).cloned();
        drop(adapters);

        let Some(adapter) = adapter else {
            return Ok(None); // fail-open: venue desconhecido
        };

        let client = self.client.clone();
        let token_cache = self.token_cache.clone();
        let ad = adapter.clone();
        let ad_liq = adapter.clone();

        crate::dex::liquidity::read_pool_tvl_usd(
            client,
            &token_cache,
            dex_name,
            token_a,
            token_b,
            fee_hint,
            move |_dex, a, b, fee| {
                let ad = ad.clone();
                async move { ad.get_pool_address_for_liquidity(a, b, fee).await }
            },
            move |_dex, a, b| {
                let ad = ad_liq.clone();
                async move { ad.liquidity_token_addresses(a, b).await }
            },
        )
        .await
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

    pub async fn get_active_adapters(&self) -> Vec<String> {
        let adapters = self.active_adapters.read().await;
        adapters.iter().map(|a| a.name().to_string()).collect()
    }
}
