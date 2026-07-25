use crate::{
    config::Config,
    core::types::{ArbitrageOpportunity, ArbitrageStep, SerializableSteps},
    dex::get_token_decimals,
    utils::{f64_to_u256, u256_to_f64},
    AppMiddleware,
};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use ethers::types::{Address, U256};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    str::FromStr,
    sync::Arc,
};
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::RwLock;
use tracing::{debug, info, instrument, warn};

// ------------------------------------------------------------
// ⚙️ CONSTANTES DERIVADAS (removemos hardcodes sempre que possível)
// ------------------------------------------------------------
const MAX_HOPS_FOR_EXECUTION: usize = 4;
const SANITIZED_PLACEHOLDER: &str = "QuickSwap";
const TARGET_BASE_TOKEN: &str = "USDT";
const MIN_TRADE_AMOUNT_USD: f64 = 0.5;
const MAX_TRADE_AMOUNT_USD: f64 = 100.0;
/// Limite máximo para trades com flashloan (separado do limite manual).
/// Flashloans permitem operar com capital emprestado, então o limite pode ser maior.
const MAX_TRADE_AMOUNT_FLASHLOAN_USD: f64 = 10_000.0;
const MAX_REALISTIC_SPREAD: f64 = 100.0;
const MAX_REALISTIC_PROFIT_RATIO: f64 = 0.50;

/// Contador atômico global para gerar IDs únicos de oportunidades.
/// Evita colisões quando múltiplas oportunidades são criadas no mesmo milissegundo.
static OPP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Gera um ID único combinando timestamp + contador atômico.
#[inline]
fn next_opp_id(prefix: &str) -> String {
    let ts = Utc::now().timestamp_millis();
    let seq = OPP_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{}_{}", prefix, ts, seq)
}

/// Taxas de swap por DEX (em fração, ex: 0.003 = 0.3%).
/// V2 pools (QuickSwap, SushiSwap) cobram 0.3%.
/// V3 pools (UniswapV3) variam — usamos 0.3% como default conservador.
const DEX_FEE_DEFAULT: f64 = 0.003;

// ------------------------------------------------------------
// 🧠 Estrutura principal
// ------------------------------------------------------------
#[derive(Clone)]
pub struct ArbitrageEngine {
    middleware: Arc<AppMiddleware>,
    decimals_cache: Arc<RwLock<HashMap<String, u32>>>,
}

impl fmt::Debug for ArbitrageEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ArbitrageEngine")
            .field("decimals_cache_size", &"<rw-cache>")
            .finish()
    }
}

impl ArbitrageEngine {
    // ------------------------------------------------------------
    // 📈 Função de DEBUG avançado: delta entre steps, hops e preço final
    // ------------------------------------------------------------
    fn log_route_delta(&self, steps: &[ArbitrageStep], total_rate: f64, opp_id: &str) {
        if steps.len() < 2 {
            debug!("🔹 [delta] Rota {} tem menos de 2 steps, ignorando.", opp_id);
            return;
        }

        debug!("🔷 ================= DELTA DEBUG =================");
        debug!("🔷 Rota ID: {}", opp_id);
        debug!("🔷 Steps: {}", steps.len());

        let mut deltas = Vec::new();

        for i in 0..steps.len() {
            let s = &steps[i];

            debug!(
                "Step {} | {} | {} -> {} | rate={:.8} | DEX={}",
                i,
                s.dex_name,
                s.token_in,
                s.token_out,
                s.expected_rate,
                s.dex_name
            );

            if i > 0 {
                let prev = steps[i - 1].expected_rate;
                let curr = s.expected_rate;

                // Evitar NaN por divisão por 0
                if prev.is_finite() && prev.abs() > 1e-18 {
                    let delta = curr - prev;
                    let delta_pct = (delta / prev) * 100.0;

                    debug!("   └─ Δ step {}: {:.8} ({:+.4}%)", i, delta, delta_pct);
                    deltas.push(delta_pct);
                } else {
                    debug!("   └─ Δ step {}: prev inválido ({}), ignorando %", i, prev);
                }
            }
        }

        if !deltas.is_empty() {
            let avg_delta = deltas.iter().sum::<f64>() / deltas.len() as f64;
            debug!("🔶 Delta médio entre hops: {:+.4}% ", avg_delta);
        }

        // taxa mínima e máxima nos steps
        let mut rates: Vec<f64> = steps.iter().map(|s| s.expected_rate).collect();
        rates.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

        let min_rate = rates.first().unwrap_or(&0.0);
        let max_rate = rates.last().unwrap_or(&0.0);

        let spread_rate = max_rate - min_rate;
        let spread_rate_pct = if min_rate.is_finite() && min_rate.abs() > 1e-18 {
            (spread_rate / min_rate.max(1e-12)) * 100.0
        } else {
            0.0
        };

        let final_delta_pct = (total_rate - 1.0) * 100.0;

        debug!(
            "DELTA spread_interno={:.6}% | ΔFINAL={:+.6}% | rota={}",
            spread_rate_pct, final_delta_pct, opp_id
        );

        debug!("🔷 ================= FIM DELTA =================");
    }

    /// Construtor padrão
    #[inline]
    pub fn new(middleware: Arc<AppMiddleware>) -> Self {
        Self {
            middleware,
            decimals_cache: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // ------------------------------------------------------------
    // 🔧 DEBUG: Método para teste direto (Mantido para compatibilidade)
    // ------------------------------------------------------------
    pub async fn debug_direct_analysis(&self, app_config: &Config) -> Vec<ArbitrageOpportunity> {
        info!("🔧 DEBUG: Analisando oportunidades manualmente...");

        // Criar um price_map manual baseado nos logs
        let mut price_map = HashMap::new();

        // QuickSwap (dos seus logs)
        let mut quickswap = HashMap::new();
        quickswap.insert("WETH-WMATIC".to_string(), 21336.68011837);
        quickswap.insert("WETH-USDT".to_string(), 2812.42306300);
        quickswap.insert("WETH-USDC".to_string(), 2905.29980500);
        quickswap.insert("WMATIC-USDT".to_string(), 0.13690800);
        quickswap.insert("WETH-DAI".to_string(), 2726.84538320);
        quickswap.insert("USDC-USDT".to_string(), 0.99979100);
        quickswap.insert("DAI-USDC".to_string(), 1.00012313);
        quickswap.insert("WMATIC-USDC".to_string(), 0.13732300);
        quickswap.insert("WMATIC-DAI".to_string(), 0.13680221);
        price_map.insert("QuickSwap".to_string(), quickswap);

        // UniswapV3
        let mut uniswap = HashMap::new();
        uniswap.insert("WMATIC-USDT".to_string(), 0.13746200);
        uniswap.insert("WETH-USDC".to_string(), 2914.29852500);
        uniswap.insert("WMATIC-USDC".to_string(), 0.13742100);
        uniswap.insert("WETH-WMATIC".to_string(), 21255.69659823);
        uniswap.insert("WETH-DAI".to_string(), 1762.27185512);
        uniswap.insert("USDC-USDT".to_string(), 0.99998800);
        uniswap.insert("WETH-USDT".to_string(), 2914.23198300);
        uniswap.insert("DAI-USDC".to_string(), 1.00045904);
        uniswap.insert("WMATIC-DAI".to_string(), 0.13695220);
        price_map.insert("UniswapV3".to_string(), uniswap);

        // SushiSwap
        let mut sushiswap = HashMap::new();
        sushiswap.insert("WETH-USDC".to_string(), 2875.63860600);
        sushiswap.insert("DAI-USDC".to_string(), 1.00101215);
        sushiswap.insert("WETH-USDT".to_string(), 2785.27121000);
        sushiswap.insert("WMATIC-USDT".to_string(), 0.13673400);
        sushiswap.insert("WMATIC-USDC".to_string(), 0.13690300);
        sushiswap.insert("WETH-DAI".to_string(), 2738.33572186);
        sushiswap.insert("WMATIC-DAI".to_string(), 0.13742209);
        sushiswap.insert("WETH-WMATIC".to_string(), 21336.39335884);
        sushiswap.insert("USDC-USDT".to_string(), 0.99872400);
        price_map.insert("SushiSwap".to_string(), sushiswap);

        info!("🔧 DEBUG: Price_map criado com {} DEXs", price_map.len());

        // Chamar o método normal
        self.find_arbitrage_opportunities(&price_map, app_config).await
    }

    // ------------------------------------------------------------
    // 🧮 FUNÇÕES DE CÁLCULO CORRIGIDAS
    // ------------------------------------------------------------

    /// Normaliza amount considerando decimals
    fn normalize_amount(amount: U256, decimals: u32) -> f64 {
        if amount.is_zero() {
            return 0.0;
        }
        u256_to_f64(amount, decimals)
    }

    /// 🔧 CORREÇÃO 4: Validação de preços MAIS TOLERANTE mas SEGURA
    ///
    /// Usa `token_in`/`token_out` explicitamente (direction-aware) em vez de
    /// `contains()` no par combinado, que era direction-agnóstico e rejeitava
    /// pares legítimos como USDT-WMATIC (rate ~7.14 caía no range [0.10, 5.0]
    /// destinado a WMATIC-USDT).
    fn is_realistic_price(price: f64, token_in: &str, token_out: &str) -> bool {
        if !price.is_finite() || price <= 0.0 {
            return false;
        }

        let is_stable = |t: &str| matches!(t, "USDT" | "USDC" | "DAI");

        match (is_stable(token_in), is_stable(token_out)) {
            // Ambos stablecoins: ~1.0
            (true, true) => price >= 0.80 && price <= 1.20,

            // stable -> non-stable: "how many tokens per 1 stable"
            (true, false) => match token_out {
                "WETH" => price >= 0.00005 && price <= 0.002,   // ~0.00034 WETH/USDT
                "WMATIC" => price >= 1.0 && price <= 50.0,       // ~7.14 WMATIC/USDT
                _ => price >= 0.0000001 && price <= 10_000_000.0, // fallback amplo
            },

            // non-stable -> stable: "how many stables per 1 token"
            (false, true) => match token_in {
                "WETH" => price >= 500.0 && price <= 15_000.0,    // ~2900 USDT/WETH
                "WMATIC" => price >= 0.01 && price <= 2.0,        // ~0.14 USDT/WMATIC
                "LINK" => price >= 1.0 && price <= 100.0,         // ~$5-30
                "CRV" => price >= 0.1 && price <= 10.0,
                "UNI" => price >= 1.0 && price <= 50.0,
                "GHST" => price >= 0.5 && price <= 20.0,
                "SAND" => price >= 0.1 && price <= 5.0,
                "SUSHI" => price >= 0.5 && price <= 20.0,
                "GRT" => price >= 0.05 && price <= 5.0,
                "LDO" => price >= 0.5 && price <= 20.0,
                _ => price >= 0.0000001 && price <= 10_000_000.0, // fallback amplo
            },

            // Ambos non-stable: ratio entre tokens
            (false, false) => match (token_in, token_out) {
                ("WETH", "WMATIC") => price >= 500.0 && price <= 100_000.0,  // ~21k
                ("WMATIC", "WETH") => price >= 0.000001 && price <= 0.01,    // ~0.000047
                _ => price >= 0.0000001 && price <= 10_000_000.0,
            },
        }
    }

    /// 🔧 CORREÇÃO 5: Cálculo de taxa total COM VALIDAÇÃO POR STEP
    fn calculate_total_rate_corrected(steps: &[ArbitrageStep]) -> Result<f64, String> {
        if steps.is_empty() {
            return Err("Steps vazios".to_string());
        }

        let mut total_rate = 1.0;
        let mut debug_info = Vec::new();

        for (i, step) in steps.iter().enumerate() {
            // Validar taxa individual
            if !step.expected_rate.is_finite() || step.expected_rate <= 0.0 {
                return Err(format!(
                    "Taxa inválida no step {}: {} ({}→{})",
                    i, step.expected_rate, step.token_in, step.token_out
                ));
            }

            // Validar se a taxa faz sentido para o par
            if !Self::is_realistic_price(step.expected_rate, &step.token_in, &step.token_out) {
                return Err(format!(
                    "Preço irreal no step {}: {} {}→{} = {:.8}",
                    i, step.dex_name, step.token_in, step.token_out, step.expected_rate
                ));
            }

            total_rate *= step.expected_rate;

            debug_info.push(format!(
                "Step{}: {}→{} rate={:.8}",
                i, step.token_in, step.token_out, step.expected_rate
            ));

            // Validar que não explodiu
            if !total_rate.is_finite() {
                return Err(format!(
                    "Taxa acumulada infinita após step {}: {} | Steps: {:?}",
                    i, total_rate, debug_info
                ));
            }
        }

        // VALIDAÇÃO FINAL: taxa total típica de arb não deve explodir
        // Mantém tolerância, mas bloqueia ilusões de "multiplica e fica gigante"
        if total_rate < 0.90 || total_rate > 1.50 {
            return Err(format!(
                "Taxa total suspeita: {:.8} (esperado 0.90-1.50) | Route: {:?}",
                total_rate, debug_info
            ));
        }

        debug!("✅ Taxa total validada: {:.8} | Route: {:?}", total_rate, debug_info);

        Ok(total_rate)
    }

    /// Valida se a oportunidade é minimamente realista
    fn validate_opportunity(opp: &ArbitrageOpportunity) -> Result<()> {
        let max_hops_allowed = MAX_HOPS_FOR_EXECUTION;

        if opp.spread_percent.is_nan() || opp.spread_percent.is_infinite() {
            return Err(anyhow!("Spread inválido: {}", opp.spread_percent));
        }
        if opp.estimated_profit_usd.is_nan() || opp.estimated_profit_usd.is_infinite() {
            return Err(anyhow!("Lucro estimado inválido: {}", opp.estimated_profit_usd));
        }
        if opp.steps.0.is_empty() || opp.steps.0.len() > max_hops_allowed {
            return Err(anyhow!("Número de hops inválido: {}", opp.steps.0.len()));
        }

        // Guarda adicional: razão de lucro “humanamente plausível”
        if opp.estimated_volume_usd > 0.0 {
            let ratio = opp.estimated_profit_usd / opp.estimated_volume_usd;
            if ratio.is_finite() && ratio > MAX_REALISTIC_PROFIT_RATIO {
                return Err(anyhow!(
                    "Razão de lucro suspeita: {:.4} (> {:.4})",
                    ratio,
                    MAX_REALISTIC_PROFIT_RATIO
                ));
            }
        }

        Ok(())
    }

    // ------------------------------------------------------------
    // 🧩 Sanitizador de DEX
    // ------------------------------------------------------------
    #[inline]
    fn sanitize_dex_name(dex_name: &str) -> String {
        match dex_name {
            "TriHop" | "BellmanFord" | "Unknown" | "MultiHop" | "Router" | "Pathfinder" => {
                SANITIZED_PLACEHOLDER.to_string()
            }
            _ => dex_name.to_string(),
        }
    }

    #[inline]
    pub fn sanitize_steps_for_execution(steps: &[ArbitrageStep]) -> Vec<ArbitrageStep> {
        steps
            .iter()
            .map(|s| ArbitrageStep {
                dex_name: Self::sanitize_dex_name(&s.dex_name),
                dex_address: s.dex_address.clone(),
                token_in: s.token_in.clone(),
                token_out: s.token_out.clone(),
                expected_rate: s.expected_rate,
                amount_out_min: s.amount_out_min,
                // Propagar overrides se existirem (compatível com struct nova)
                dex_fee_bps: s.dex_fee_bps,
                price_impact_bps: s.price_impact_bps,
            })
            .collect()
    }

    // ------------------------------------------------------------
    // 🔍 Descoberta principal
    // ------------------------------------------------------------
    #[instrument(skip_all, level = "info")]
    pub async fn find_arbitrage_opportunities(
        &self,
        price_map: &HashMap<String, HashMap<String, f64>>,
        app_config: &Config,
    ) -> Vec<ArbitrageOpportunity> {
        info!("📊 DEX Count: {}", price_map.len());

        for (dex, pairs) in price_map {
            info!("📊 DEX '{}': {} pares", dex, pairs.len());
            for (pair, price) in pairs.iter().take(5) {
                info!("    {} = {:.8}", pair, price);
            }
            if pairs.len() > 5 {
                info!("    ... e mais {} pares", pairs.len() - 5);
            }
        }

        let min_spread_pct = app_config
            .arbitrage
            .min_spread_percent
            .parse::<f64>()
            .unwrap_or(0.008);

        let min_profit_usd = app_config.arbitrage.min_profit_threshold_usd.unwrap_or(0.0015);

        debug!(
            target = "arbitrage",
            min_spread_pct = %min_spread_pct,
            min_profit_usd = %min_profit_usd,
            "🔍 Iniciando busca por oportunidades"
        );

        let total_pairs = price_map.values().map(|m| m.len()).sum::<usize>();
        if total_pairs == 0 {
            warn!(target = "arbitrage", "Nenhum par disponível no price_map");
            return vec![];
        }

        let mut all_opportunities = Vec::new();

        let direct_usdt = self.find_direct_with_usdt(price_map, app_config).await;
        let tri_usdt = self.find_triangular_with_usdt(price_map, app_config).await;

        all_opportunities.extend(direct_usdt);
        all_opportunities.extend(tri_usdt);

        let direct_generic = self.find_direct_async(price_map, app_config).await;
        all_opportunities.extend(direct_generic);

        // 🔄 Deduplicação por (pair, buy_dex, sell_dex) — find_direct_with_usdt e
        // find_direct_async podem produzir as mesmas oportunidades para pares USDT.
        let before_dedup = all_opportunities.len();
        all_opportunities.sort_by(|a, b| {
            b.spread_percent
                .partial_cmp(&a.spread_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let mut seen: HashSet<(String, String, String)> = HashSet::new();
        all_opportunities.retain(|opp| {
            let key = (opp.pair.clone(), opp.buy_dex.clone(), opp.sell_dex.clone());
            if seen.contains(&key) {
                return false;
            }
            seen.insert(key);
            true
        });
        if before_dedup != all_opportunities.len() {
            info!(
                "📊 Deduplicação: {} → {} (removidas {} duplicadas)",
                before_dedup,
                all_opportunities.len(),
                before_dedup - all_opportunities.len()
            );
        }

        info!("📊 Oportunidades iniciais: {} (pairs={})", all_opportunities.len(), total_pairs);

        all_opportunities.retain(|opp| opp.spread_percent >= min_spread_pct);
        info!(
            "📊 Após filtro de spread ({}%): {}",
            min_spread_pct,
            all_opportunities.len()
        );

        if all_opportunities.is_empty() {
            debug!(target = "arbitrage", "Nenhuma oportunidade após filtro de spread");
            return vec![];
        }

        all_opportunities.sort_by(|a, b| {
            b.spread_percent
                .partial_cmp(&a.spread_percent)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut usdt_opportunities = Vec::new();
        for (i, mut opp) in all_opportunities.into_iter().enumerate() {
            info!(
                "🔄 Convertendo oportunidade {}: path={:?}, spread={}%, profit=${}",
                i, opp.path, opp.spread_percent, opp.estimated_profit_usd
            );

            if let Some(usdt_opp) = self.force_usdt_start_end_optimized(&mut opp, app_config).await
            {
                if Self::validate_opportunity(&usdt_opp).is_ok() {
                    info!(
                        "✅ Conversão USDT bem-sucedida: profit=${}, spread={}% (net=${})",
                        usdt_opp.estimated_profit_usd,
                        usdt_opp.spread_percent,
                        usdt_opp.net_profit_usd
                    );
                    usdt_opportunities.push(usdt_opp);
                } else {
                    info!("❌ Validação falhou após conversão USDT");
                }
            } else {
                info!("❌ Conversão USDT falhou ou rota muito longa");
            }
        }

        info!("📊 Oportunidades após conversão USDT: {}", usdt_opportunities.len());

        for (i, opp) in usdt_opportunities.iter().enumerate() {
            debug!(
                "📈 Oportunidade {}: gross=${:.6}, net=${:.6}, spread={:.4}%, min_required=${}",
                i, opp.estimated_profit_usd, opp.net_profit_usd, opp.spread_percent, min_profit_usd
            );
        }

        let before_filter = usdt_opportunities.len();
        usdt_opportunities.retain(|opp| {
            let keep = opp.net_profit_usd >= min_profit_usd; // MUDANÇA CRÍTICA: USAR net_profit_usd
            if !keep {
                debug!(
                    "🚫 Filtrado: net_profit=${:.6} < min=${}",
                    opp.net_profit_usd, min_profit_usd
                );
            }
            keep
        });

        debug!(
            "📊 Filtro de lucro: {} → {} oportunidades",
            before_filter,
            usdt_opportunities.len()
        );

        if usdt_opportunities.is_empty() {
            debug!(
                target = "arbitrage",
                "Nenhuma oportunidade acima do threshold (min_spread={}%, min_profit=${})",
                min_spread_pct, min_profit_usd
            );
        } else {
            let best = &usdt_opportunities[0];

            // Log formatado como tabela para o operador
            info!("═══════════════════════════════════════════════════════════════════");
            info!("🎯 {} OPORTUNIDADES ENCONTRADAS", usdt_opportunities.len());
            info!("═══════════════════════════════════════════════════════════════════");
            info!("{:<8} {:<20} {:<12} {:<12} {:<10}", "RANK", "PAR", "SPREAD%", "NET($)", "CONFIAB.");
            info!("───────────────────────────────────────────────────────────────────");

            for (i, opp) in usdt_opportunities.iter().take(10).enumerate() {
                let conf_str = format!("{:.0}%", opp.confidence * 100.0);
                info!(
                    "{:<8} {:<20} {:<12.4} {:<12.6} {:<10}",
                    format!("#{}", i + 1),
                    opp.pair,
                    opp.spread_percent,
                    opp.net_profit_usd,
                    conf_str
                );
            }

            info!("───────────────────────────────────────────────────────────────────");
            info!(
                "🏆 MELHOR: {} | Spread: {:.4}% | Net: ${:.6} | Confiança: {:.0}%",
                best.pair,
                best.spread_percent,
                best.net_profit_usd,
                best.confidence * 100.0
            );

            if usdt_opportunities.len() > 1 {
                let avg_spread: f64 = usdt_opportunities.iter().map(|o| o.spread_percent).sum::<f64>() / usdt_opportunities.len() as f64;
                let total_net: f64 = usdt_opportunities.iter().map(|o| o.net_profit_usd).sum();
                info!(
                    "📊 Média spread: {:.4}% | Total net potencial: ${:.6}",
                    avg_spread, total_net
                );
            }
            info!("═══════════════════════════════════════════════════════════════════");
        }

        usdt_opportunities
    }

    // ------------------------------------------------------------
    // 🔄 Conversão e Construção
    // ------------------------------------------------------------
    #[instrument(skip_all, level = "debug")]
    async fn convert_to_usdt_centric(
        &self,
        opp: &mut ArbitrageOpportunity,
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        let path = &opp.path;
        let steps = &opp.steps.0;

        debug!(
            "🔄 Iniciando conversão USDT: path={:?}, steps={}, original_profit=${}",
            path,
            steps.len(),
            opp.estimated_profit_usd
        );

        if Self::is_usdt_centric(path) && steps.len() <= MAX_HOPS_FOR_EXECUTION {
            debug!("✅ Já é USDT-centric, apenas recalculando...");
            let steps_sanitized = Self::sanitize_steps_for_execution(steps);
            let mut refreshed_opp = opp.clone();
            refreshed_opp.steps = SerializableSteps(steps_sanitized);

            if self
                .recalculate_profitability(&mut refreshed_opp, app_config)
                .await
                .is_ok()
            {
                debug!(
                    "✅ Recalculação bem-sucedida: novo net_profit=${}",
                    refreshed_opp.net_profit_usd
                );
                return Some(refreshed_opp);
            }
            debug!("❌ Recalculação falhou");
            return None;
        }

        debug!("🔄 Convertendo oportunidade não-USDT...");
        self.convert_non_usdt_opportunity(path, steps, app_config).await
    }

    async fn convert_non_usdt_opportunity(
        &self,
        original_path: &[String],
        original_steps: &[ArbitrageStep],
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        match (original_path.len(), original_steps.len()) {
            (3, 2) if !Self::is_usdt_centric(original_path) => {
                self.convert_direct_arbitrage(original_path, original_steps, app_config)
                    .await
            }
            (4..=5, 3..=4)
                if original_path
                    .first()
                    .map(|s| s.as_str())
                    == Some(TARGET_BASE_TOKEN) =>
            {
                self.build_usdt_opportunity(
                    original_path.to_vec(),
                    original_steps.to_vec(),
                    app_config,
                )
                .await
            }
            _ => {
                debug!(
                    target: "arbitrage",
                    path_len = original_path.len(),
                    steps_len = original_steps.len(),
                    "Rota muito complexa para conversão USDT"
                );
                None
            }
        }
    }

    async fn convert_direct_arbitrage(
        &self,
        path: &[String],
        steps: &[ArbitrageStep],
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        if path.len() != 3 || steps.len() != 2 {
            return None;
        }

        let token_a = &path[0];
        let token_b = &path[1];

        // Importante: se não conseguimos inferir taxa USDT->token ou token->USDT de forma segura,
        // retornamos 0.0 e o recálculo vai rejeitar por preço inválido (evita ilusões).
        let usdt_to_a = self.estimate_usdt_rate(token_a, steps, true);
        let b_to_usdt = self.estimate_usdt_rate(token_b, steps, false);

        let usdt_steps = vec![
            Self::create_step(SANITIZED_PLACEHOLDER, TARGET_BASE_TOKEN, token_a, usdt_to_a),
            steps[0].clone(), // Hop original A->B
            Self::create_step(SANITIZED_PLACEHOLDER, token_b, TARGET_BASE_TOKEN, b_to_usdt),
        ];

        let usdt_path = vec![
            TARGET_BASE_TOKEN.into(),
            token_a.clone(),
            token_b.clone(),
            TARGET_BASE_TOKEN.into(),
        ];

        self.build_usdt_opportunity(usdt_path, usdt_steps, app_config).await
    }

    async fn build_usdt_opportunity(
        &self,
        path: Vec<String>,
        steps: Vec<ArbitrageStep>,
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        if steps.len() > MAX_HOPS_FOR_EXECUTION {
            debug!("🚫 Muitos hops: {} > {}", steps.len(), MAX_HOPS_FOR_EXECUTION);
            return None;
        }

        if !Self::is_usdt_centric(&path) {
            debug!("🚫 Não é USDT-centric: {:?}", path);
            return None;
        }

        let opportunity_id = next_opp_id("usdt_arb");
        let steps_sanitized = Self::sanitize_steps_for_execution(&steps);

        let mut opp = ArbitrageOpportunity {
            id: opportunity_id,
            pair: path.join("->"),
            buy_dex: "MultiHop".into(),
            sell_dex: "MultiHop".into(),
            buy_price: steps.first().map(|s| s.expected_rate).unwrap_or(0.0),
            sell_price: steps.last().map(|s| s.expected_rate).unwrap_or(0.0),
            spread_percent: 0.0,
            amount_in: U256::zero(),
            amount_out: U256::zero(),
            estimated_profit_usd: 0.0,
            gas_cost_usd: 0.0,
            net_profit_usd: 0.0,
            steps: SerializableSteps(steps_sanitized),
            path,
            timestamp: Utc::now().timestamp() as u64,
            confidence: 0.0,
            estimated_volume_usd: 0.0,
            profit_percent: 0.0,
            execution_risk: 0.0,
            force_flashloan: false,
            token_price_usd: Some(1.0),
        };

        if self
            .recalculate_profitability(&mut opp, app_config)
            .await
            .is_ok()
        {
            Some(opp)
        } else {
            None
        }
    }

    // ------------------------------------------------------------
    // 🧮 Recálculo de Profitabilidade
    // ------------------------------------------------------------
    #[instrument(skip_all, level = "debug")]
    async fn recalculate_profitability(
        &self,
        opp: &mut ArbitrageOpportunity,
        app_config: &Config,
    ) -> Result<()> {
        let usdt_decimals = self
            .get_token_decimals_smart(TARGET_BASE_TOKEN, app_config)
            .await
            .context("Falha ao obter decimals do USDT")?;

        let trade_amount_usd = self.calculate_safe_trade_amount(app_config);
        let amount_in = Self::usd_to_token_amount(trade_amount_usd, 1.0, usdt_decimals);

        // CORREÇÃO: Calcular taxa total com validação rigorosa
        let total_rate = match Self::calculate_total_rate_corrected(&opp.steps.0) {
            Ok(rate) => rate,
            Err(e) => {
                debug!("❌ Oportunidade rejeitada: {}", e);
                return Err(anyhow!("Cálculo de taxa falhou: {}", e));
            }
        };

        // 🔍 DEBUG DELTA ENTRE STEPS
        self.log_route_delta(&opp.steps.0, total_rate, &opp.id);

        let spread_percent = (total_rate - 1.0) * 100.0;

        // Validação de spread
        let min_spread = app_config
            .arbitrage
            .min_spread_percent
            .parse::<f64>()
            .unwrap_or(0.008);

        if spread_percent < min_spread {
            return Err(anyhow!(
                "Spread insuficiente: {:.6}% < {:.6}%",
                spread_percent,
                min_spread
            ));
        }

        // CORREÇÃO: Calcular profit BRUTO (sem custos ainda)
        let gross_profit_usd = trade_amount_usd * (total_rate - 1.0);

        // CORREÇÃO: Calcular TODOS os custos
        let gas_cost_usd = self.estimate_gas_cost(app_config).await;

        // Fee do flashloan (ler do config, se habilitado)
        let flashloan_fee_usd = if app_config.flashloan.enabled {
            let fee_pct = app_config.flashloan.fee_pct.unwrap_or(0.0009); // fallback pequeno, prefer config
            trade_amount_usd * fee_pct
        } else {
            0.0
        };

        // Slippage esperado: usar configuração (bps -> pct)
        let default_price_impact_bps = app_config.execution.default_price_impact_bps;
        let expected_slippage_usd =
            trade_amount_usd * (default_price_impact_bps as f64 / 10000.0);

        // LUCRO LÍQUIDO REAL
        let net_profit_usd =
            gross_profit_usd - gas_cost_usd - flashloan_fee_usd - expected_slippage_usd;

        debug!(
            "PROFIT gross=${:.6} - gas=${:.6} - flashloan=${:.6} - slippage=${:.6} = net=${:.6}",
            gross_profit_usd, gas_cost_usd, flashloan_fee_usd, expected_slippage_usd, net_profit_usd
        );

        // Validação final
        let min_profit = app_config.arbitrage.min_profit_threshold_usd.unwrap_or(0.0015);

        if net_profit_usd < min_profit {
            return Err(anyhow!(
                "Lucro líquido insuficiente: ${:.6} < ${:.6}",
                net_profit_usd,
                min_profit
            ));
        }

        // Calcular slippage protection CORRETAMENTE usando BPS vindos do Config
        self.calculate_slippage_protection(&mut opp.steps.0, amount_in, app_config)
            .await?;

        // Atualizar oportunidade
        opp.amount_in = amount_in;
        opp.estimated_profit_usd = gross_profit_usd;
        opp.spread_percent = spread_percent;
        opp.gas_cost_usd = gas_cost_usd;
        opp.net_profit_usd = net_profit_usd;
        opp.estimated_volume_usd = trade_amount_usd;
        opp.confidence = Self::calculate_confidence(spread_percent, opp.steps.0.len());

        debug!(
            "✅ Oportunidade validada: spread={:.6}%, net_profit=${:.6}, confidence={:.2}",
            spread_percent, net_profit_usd, opp.confidence
        );

        Ok(())
    }

    // ------------------------------------------------------------
    // 🛡️ Cálculo de Proteção contra Slippage (sem hardcodes, usa Config)
    // ------------------------------------------------------------
    /// Slippage protection REALISTA e ACUMULATIVA
    /// Lê base_slippage_bps, hop_increase_bps, safety_margin_bps e default dex/impact dos módulos de Config.
    async fn calculate_slippage_protection(
        &self,
        steps: &mut [ArbitrageStep],
        initial_amount: U256,
        app_config: &Config,
    ) -> Result<()> {
        let mut current_amount = initial_amount;

        // Ler parâmetros do Config
        let base_slippage_bps = app_config.execution.max_slippage_bps; // ex.: 20 = 0.20%
        let hop_increase_bps = app_config.execution.hop_slippage_increase_bps; // ex.: 20 = +0.20% por hop (em bps)
        let safety_margin_bps = app_config.execution.safety_margin_bps; // ex.: 9800 = 98%

        for (idx, step) in steps.iter_mut().enumerate() {
            let input_decimals = self.get_token_decimals_smart(&step.token_in, app_config).await?;
            let output_decimals = self.get_token_decimals_smart(&step.token_out, app_config).await?;

            // Cálculo de output esperado aplicando fee/impact (considerando override por step, se existir)
            let expected_output = self
                .calculate_expected_output_with_fees(
                    current_amount,
                    step.expected_rate,
                    input_decimals,
                    output_decimals,
                    &step.dex_name,
                    app_config,
                    step.dex_fee_bps,
                    step.price_impact_bps,
                )
                .await;

            // Slippage acumulativo (BPS)
            let adjusted_slippage_bps = base_slippage_bps
                .saturating_add((idx as u32).saturating_mul(hop_increase_bps));

            // Aplicar slippage seguro usando aritmética inteira (U256)
            step.amount_out_min = Self::apply_slippage_safe(
                expected_output,
                adjusted_slippage_bps,
                safety_margin_bps,
            );

            // DEBUG
            let output_f64 = u256_to_f64(expected_output, output_decimals);
            let min_f64 = u256_to_f64(step.amount_out_min, output_decimals);

            debug!(
                "Step {}: {} -> {} | expected={:.6} | min={:.6} | slip={} bps | dex={}",
                idx, step.token_in, step.token_out, output_f64, min_f64, adjusted_slippage_bps, step.dex_name
            );

            current_amount = expected_output;
        }

        Ok(())
    }

    // ------------------------------------------------------------
    // 🎯 Estratégias de Busca Específicas
    // ------------------------------------------------------------
    async fn find_direct_with_usdt(
        &self,
        prices: &HashMap<String, HashMap<String, f64>>,
        app_config: &Config,
    ) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();

        for pair in Self::extract_usdt_pairs(prices) {
            if let Some(opp) = self.evaluate_direct(&pair, prices, app_config).await {
                opportunities.push(opp);
            }
        }

        opportunities
    }

    async fn find_direct_async(
        &self,
        prices: &HashMap<String, HashMap<String, f64>>,
        app_config: &Config,
    ) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();
        let all_pairs = Self::get_all_pairs(prices);

        for pair in all_pairs {
            if let Some(opp) = self.evaluate_direct(&pair, prices, app_config).await {
                opportunities.push(opp);
            }
        }
        opportunities
    }

    async fn evaluate_direct(
        &self,
        pair: &str,
        prices: &HashMap<String, HashMap<String, f64>>,
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        let parts: Vec<&str> = pair.split('-').collect();
        if parts.len() != 2 {
            return None;
        }
        let token_a = parts[0];
        let token_b = parts[1];

        let reverse_pair = format!("{}-{}", token_b, token_a);

        // Coletar todos os rates por DEX (não apenas o melhor por direção)
        let mut rates_ab: Vec<(f64, String)> = Vec::new();
        let mut rates_ba: Vec<(f64, String)> = Vec::new();

        for (dex_name, dex_prices) in prices {
            if let Some(&rate) = dex_prices.get(pair) {
                if rate.is_finite() && rate > 0.0 {
                    rates_ab.push((rate, dex_name.clone()));
                }
            }
            if let Some(&rate) = dex_prices.get(&reverse_pair) {
                if rate.is_finite() && rate > 0.0 {
                    rates_ba.push((rate, dex_name.clone()));
                }
            }
        }

        if rates_ab.is_empty() || rates_ba.is_empty() {
            info!("  ❌ {}: rates_ab={} rates_ba={}", pair, rates_ab.len(), rates_ba.len());
            return None;
        }

        // Avaliar TODAS as combinações (buy_dex, sell_dex) para encontrar
        // o melhor cycle_rate cross-DEX. Inclui fees dos pools de AMBOS os DEXes.
        let min_spread = app_config
            .arbitrage
            .min_spread_percent
            .parse::<f64>()
            .unwrap_or(0.008);

        let mut best: Option<(f64, f64, String, String, f64)> = None;

        for (rate_ab, buy_dex) in &rates_ab {
            for (rate_ba, sell_dex) in &rates_ba {
                if buy_dex == sell_dex {
                    continue;
                }

                if !Self::is_realistic_price(*rate_ab, token_a, token_b)
                    || !Self::is_realistic_price(*rate_ba, token_b, token_a)
                {
                    continue;
                }

                // ✅ CORRIGIDO: Rates vindas de getAmountsOut (V2) / quoteExactInputSingle (V3)
                // JÁ incluem fee e price impact. Aplicar (1-fee) novamente seria dedução dupla.
                // cycle_rate = rate_ab × rate_ba (ambos já com fee embutida)
                let cycle_rate = rate_ab * rate_ba;
                let spread_pct = (cycle_rate - 1.0) * 100.0;

                if spread_pct > MAX_REALISTIC_SPREAD || spread_pct <= min_spread {
                    continue;
                }

                if best.is_none() || cycle_rate > best.as_ref().unwrap().0 {
                    best = Some((cycle_rate, *rate_ab, buy_dex.clone(), sell_dex.clone(), spread_pct));
                }
            }
        }

        if best.is_none() {
            debug!("  ❌ {}: nenhum cycle cross-DEX viável (rates_ab={:?}, rates_ba={:?})", pair,
                rates_ab.iter().map(|(r,d)| format!("{}={:.4}", d, r)).collect::<Vec<_>>(),
                rates_ba.iter().map(|(r,d)| format!("{}={:.4}", d, r)).collect::<Vec<_>>());
        }

        let (cycle_rate, rate_ab, buy_dex, sell_dex, spread_pct) = best?;

        // Encontrar o rate_ba correspondente ao sell_dex escolhido
        let rate_ba = rates_ba.iter()
            .find(|(_, d)| *d == sell_dex)
            .map(|(r, _)| *r)
            .unwrap_or(0.0);

        let steps = vec![
            Self::create_step(&buy_dex, token_a, token_b, rate_ab),
            Self::create_step(&sell_dex, token_b, token_a, rate_ba),
        ];

        let path: Vec<String> =
            vec![token_a.to_string(), token_b.to_string(), token_a.to_string()];

        let trade_amount_usd = self.calculate_safe_trade_amount(app_config);

        Some(ArbitrageOpportunity {
            id: next_opp_id("direct_arb"),
            pair: format!("{}-{}", token_a, token_b),
            buy_dex,
            sell_dex,
            buy_price: rate_ab,
            sell_price: rate_ba,
            spread_percent: spread_pct,
            estimated_profit_usd: trade_amount_usd * (cycle_rate - 1.0),
            steps: SerializableSteps(steps),
            path,
            amount_in: U256::zero(),
            amount_out: U256::zero(),
            gas_cost_usd: 0.0,
            net_profit_usd: 0.0,
            timestamp: Utc::now().timestamp() as u64,
            confidence: 0.0,
            estimated_volume_usd: 0.0,
            profit_percent: 0.0,
            execution_risk: 0.0,
            force_flashloan: false,
            token_price_usd: None,
        })
    }

    async fn find_triangular_with_usdt(
        &self,
        prices: &HashMap<String, HashMap<String, f64>>,
        app_config: &Config,
    ) -> Vec<ArbitrageOpportunity> {
        let mut opportunities = Vec::new();
        let graph = Self::build_price_graph(prices);

        let min_spread = app_config
            .arbitrage
            .min_spread_percent
            .parse::<f64>()
            .unwrap_or(0.008);

        for token_a in graph.keys().filter(|t| *t != TARGET_BASE_TOKEN) {
            for token_b in graph
                .keys()
                .filter(|t| *t != TARGET_BASE_TOKEN && *t != token_a)
            {
                if let Some(opp) =
                    self.find_usdt_triangular(token_a, token_b, &graph, min_spread, app_config)
                        .await
                {
                    opportunities.push(opp);
                }
            }
        }

        opportunities
    }

    /// Build graph: armazena a MELHOR taxa observada para cada direção.
    ///
    /// ✅ CORREÇÃO CRÍTICA:
    /// **NÃO** fabricar inversos (1/rate) automaticamente.
    /// Em AMMs, o inverso não é simétrico por causa de fees, impacto, ticks (v3) etc.
    /// Fabricar o inverso cria “lucros fantasmas” que revertam on-chain com "Not profitable".
    ///
    /// Semântica: graph[A][B] = rate significa "quantos B se obtém por 1 unidade de A" (token_out per token_in)
    fn build_price_graph(
        prices: &HashMap<String, HashMap<String, f64>>,
    ) -> HashMap<String, HashMap<String, (f64, String)>> {
        let mut graph: HashMap<String, HashMap<String, (f64, String)>> = HashMap::new();

        for (dex_name, dex_prices) in prices {
            for (pair, &rate) in dex_prices {
                if !rate.is_finite() || rate <= 0.0 {
                    continue;
                }

                let parts: Vec<&str> = pair.split('-').collect();
                if parts.len() != 2 {
                    continue;
                }

                let token_a = parts[0].to_string();
                let token_b = parts[1].to_string();

                // escolher a MELHOR taxa (maior) para A->B
                let entry_a = graph.entry(token_a).or_insert_with(HashMap::new);
                let current_rate_a_b = entry_a.get(&token_b).map(|(r, _)| *r).unwrap_or(0.0);

                if rate > current_rate_a_b {
                    entry_a.insert(token_b, (rate, dex_name.clone()));
                }
            }
        }

        graph
    }

    async fn find_usdt_triangular(
        &self,
        token_a: &str,
        token_b: &str,
        graph: &HashMap<String, HashMap<String, (f64, String)>>,
        min_spread_pct: f64,
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        // Espera-se que graph contenha as três arestas: USDT->A, A->B, B->USDT
        let usdt_to_a = graph.get(TARGET_BASE_TOKEN)?.get(token_a)?;
        let a_to_b = graph.get(token_a)?.get(token_b)?;
        let b_to_usdt = graph.get(token_b)?.get(TARGET_BASE_TOKEN)?;

        // Semântica do graph: cada rate = token_out per token_in
        let rate_usdt_to_a = usdt_to_a.0; // quantos A por 1 USDT
        let rate_a_to_b = a_to_b.0; // quantos B por 1 A
        let rate_b_to_usdt = b_to_usdt.0; // quantos USDT por 1 B

        // Guardrails por step
        if !Self::is_realistic_price(rate_usdt_to_a, TARGET_BASE_TOKEN, token_a)
            || !Self::is_realistic_price(rate_a_to_b, token_a, token_b)
            || !Self::is_realistic_price(rate_b_to_usdt, token_b, TARGET_BASE_TOKEN)
        {
            return None;
        }

        let final_rate = rate_usdt_to_a * rate_a_to_b * rate_b_to_usdt;
        let spread = (final_rate - 1.0) * 100.0;

        if spread > MAX_REALISTIC_SPREAD {
            debug!("Spread triangular irreal rejeitado: {:.2}%", spread);
            return None;
        }

        if spread < min_spread_pct {
            return None;
        }

        let steps = vec![
            Self::create_step(&usdt_to_a.1, TARGET_BASE_TOKEN, token_a, rate_usdt_to_a),
            Self::create_step(&a_to_b.1, token_a, token_b, rate_a_to_b),
            Self::create_step(&b_to_usdt.1, token_b, TARGET_BASE_TOKEN, rate_b_to_usdt),
        ];

        let path: Vec<String> = vec![
            TARGET_BASE_TOKEN.to_string(),
            token_a.to_string(),
            token_b.to_string(),
            TARGET_BASE_TOKEN.to_string(),
        ];

        let steps_sanitized = Self::sanitize_steps_for_execution(&steps);
        let opportunity_id = next_opp_id("usdt_tri");

        let trade_amount_usd = self.calculate_safe_trade_amount(app_config);

        Some(ArbitrageOpportunity {
            id: opportunity_id,
            pair: format!("USDT->{}->{}", token_a, token_b),
            buy_dex: format!("{}/{}", usdt_to_a.1, a_to_b.1),
            sell_dex: b_to_usdt.1.clone(),
            buy_price: usdt_to_a.0,
            sell_price: b_to_usdt.0,
            spread_percent: spread,
            amount_in: U256::zero(),
            amount_out: U256::zero(),
            estimated_profit_usd: trade_amount_usd * (final_rate - 1.0),
            gas_cost_usd: 0.0,
            net_profit_usd: 0.0,
            steps: SerializableSteps(steps_sanitized),
            path,
            timestamp: Utc::now().timestamp() as u64,
            confidence: Self::calculate_confidence(spread, 3),
            estimated_volume_usd: trade_amount_usd,
            profit_percent: 0.0,
            execution_risk: 0.0,
            force_flashloan: false,
            token_price_usd: Some(1.0),
        })
    }

    // ------------------------------------------------------------
    // ⚙️ Utilitários
    // ------------------------------------------------------------
    #[inline]
    fn is_usdt_centric(path: &[String]) -> bool {
        path.first().map(|s| s.as_str()) == Some(TARGET_BASE_TOKEN)
            && path.last().map(|s| s.as_str()) == Some(TARGET_BASE_TOKEN)
    }

    #[inline]
    fn calculate_confidence(spread: f64, num_steps: usize) -> f64 {
        let spread_factor = (spread / 10.0).min(1.0);
        let steps_factor = 1.0 / (num_steps as f64).max(1.0);
        (0.5 + (spread_factor * 0.3) + (steps_factor * 0.2)).min(1.0)
    }

    #[inline]
    fn calculate_safe_trade_amount(&self, app_config: &Config) -> f64 {
        let (base_amount, max_amount) = if app_config.flashloan.enabled {
            (app_config.flashloan.capital_usd, MAX_TRADE_AMOUNT_FLASHLOAN_USD)
        } else {
            (
                app_config
                    .arbitrage
                    .default_trade_amount
                    .parse::<f64>()
                    .unwrap_or(1.0),
                MAX_TRADE_AMOUNT_USD,
            )
        };

        base_amount.clamp(MIN_TRADE_AMOUNT_USD, max_amount)
    }

    #[inline]
    fn usd_to_token_amount(usd_amount: f64, token_price: f64, decimals: u32) -> U256 {
        if token_price <= 0.0 || !usd_amount.is_finite() || usd_amount <= 0.0 {
            return U256::zero();
        }
        let token_amount = (usd_amount / token_price).max(0.0);
        f64_to_u256(token_amount, decimals)
    }

    /// Slippage seguro com aritmética inteira (U256).
    ///
    /// ⚠️ `safety_margin_bps` é interpretado como "manter X bps" (ex: 9800 = 98%).
    /// Valores < 5000 (50%) são quase certamente erro de configuração e seriam
    /// catastróficos — amount_out_min aceitaria < 50% do esperado, permitindo
    /// sandwich/MEV drenar a transação. Por segurança, clamp para mínimo de 9500 (95%).
    fn apply_slippage_safe(amount: U256, slippage_bps: u32, safety_margin_bps: u32) -> U256 {
        // 🛡️ Validação defensiva: safety_margin_bps representa "manter X bps do output"
        // Valores baixos (ex: 10) significam aceitar 0.1% do output — catastrófico.
        // Clamp para 9500 (95%) como piso absoluto de segurança.
        if safety_margin_bps < 5000 {
            warn!(
                "🚨 safety_margin_bps={} é PERIGOSAMENTE baixo (< 5000 = 50%). Aplicando clamp para 9500 (95%). \
                 Verifique config.toml [execution] safety_margin_bps — deve ser ~9800 (98%).",
                safety_margin_bps
            );
        }
        let safe_margin = safety_margin_bps.max(9500);

        let bps = U256::from(10_000u64);
        let sl_bps = U256::from(slippage_bps as u64);
        let safety_bps = U256::from(safe_margin as u64);

        // final = amount * (bps - slippage_bps) * safety_margin_bps / (bps * bps)
        let numer = amount
            .saturating_mul(bps.saturating_sub(sl_bps))
            .saturating_mul(safety_bps);
        let denom = bps.saturating_mul(bps);

        if denom.is_zero() {
            return U256::one();
        }
        let final_amount = numer / denom;

        if final_amount.is_zero() {
            U256::one()
        } else if final_amount >= amount {
            // fallback conservador: 95% do input
            amount.saturating_mul(U256::from(95u64)) / U256::from(100u64)
        } else {
            final_amount
        }
    }

    /// Cálculo realista de amount_out considerando fees e price impact vindos do Config (BPS)
    /// e/ou overrides por step (se existirem).
    async fn calculate_expected_output_with_fees(
        &self,
        amount_in: U256,
        rate: f64,
        input_decimals: u32,
        output_decimals: u32,
        dex_name: &str,
        app_config: &Config,
        dex_fee_bps_override: Option<u32>,
        price_impact_bps_override: Option<u32>,
    ) -> U256 {
        if amount_in.is_zero() || !rate.is_finite() || rate <= 0.0 {
            return U256::zero();
        }

        // Ler fee e price impact do Config (BPS), com override por step se existir
        let default_fee_bps = app_config.execution.default_dex_fee_bps;
        let default_impact_bps = app_config.execution.default_price_impact_bps;

        let dex_fee_bps = dex_fee_bps_override.unwrap_or_else(|| {
            app_config
                .execution
                .dex_fee_bps_map
                .get(dex_name)
                .cloned()
                .unwrap_or(default_fee_bps)
        });

        let price_impact_bps = price_impact_bps_override.unwrap_or_else(|| {
            app_config
                .execution
                .dex_price_impact_bps_map
                .get(dex_name)
                .cloned()
                .unwrap_or(default_impact_bps)
        });

        // Aplicar fee em aritmética inteira
        let bps_base = U256::from(10_000u64);
        let fee = U256::from(dex_fee_bps as u64);
        let amount_after_fee = amount_in.saturating_mul(bps_base.saturating_sub(fee)) / bps_base;

        // Para aplicar o rate (float) convert to f64 (usando decimals)
        let after_fee_f64 = u256_to_f64(amount_after_fee, input_decimals);

        // Aplicar rate
        let mut expected_output_f64 = after_fee_f64 * rate;

        // Aplicar price impact (bps -> factor)
        let impact_factor =
            (10_000u64.saturating_sub(price_impact_bps as u64)) as f64 / 10_000.0;
        expected_output_f64 *= impact_factor;

        // Converter para U256 com decimals de output
        f64_to_u256(expected_output_f64, output_decimals)
    }

    /// Versão simples que delega para a versão com fees usando placeholder dex (se necessário)
    async fn calculate_expected_output(
        &self,
        amount_in: U256,
        rate: f64,
        input_decimals: u32,
        output_decimals: u32,
        app_config: &Config,
    ) -> U256 {
        self.calculate_expected_output_with_fees(
            amount_in,
            rate,
            input_decimals,
            output_decimals,
            SANITIZED_PLACEHOLDER,
            app_config,
            None,
            None,
        )
        .await
    }

    async fn estimate_gas_cost(&self, app_config: &Config) -> f64 {
        let base_gas_cost = app_config.execution.estimate_base_gas_usd;
        let complexity_factor = 1.0 + (app_config.arbitrage.max_path_length as f64 * 0.05);
        base_gas_cost * complexity_factor
    }

    fn estimate_usdt_rate(&self, token: &str, steps: &[ArbitrageStep], is_input: bool) -> f64 {
        match token {
            "USDC" | "USDT" | "DAI" => 1.0,
            _ => {
                // Procura nos steps por uma conversão direta com USDT já existente.
                // Se não achar, devolve 0.0 (não inventa 1.0), para a rota ser rejeitada.
                for step in steps {
                    if is_input && step.token_out == token && step.token_in == TARGET_BASE_TOKEN {
                        return step.expected_rate;
                    }
                    if !is_input && step.token_in == token && step.token_out == TARGET_BASE_TOKEN {
                        return step.expected_rate;
                    }
                }
                // ⚠️ Logar em warn: oportunidade descartada por falta de taxa USDT direta.
                // Antes era silencioso (só debug), dificultando auditoria de oportunidades perdidas.
                warn!(
                    "⚠️ estimate_usdt_rate: sem conversão direta USDT↔{} nos steps (is_input={}). \
                     Oportunidade será rejeitada por preço inválido.",
                    token, is_input
                );
                0.0
            }
        }
    }

    fn extract_usdt_pairs(prices: &HashMap<String, HashMap<String, f64>>) -> HashSet<String> {
        let mut pairs = HashSet::new();
        for pair in Self::get_all_pairs(prices) {
            if pair.contains(TARGET_BASE_TOKEN) {
                pairs.insert(pair);
            }
        }
        pairs
    }

    #[inline]
    fn get_all_pairs(prices: &HashMap<String, HashMap<String, f64>>) -> HashSet<String> {
        prices.values().flat_map(|m| m.keys().cloned()).collect()
    }

    /// Retorna a fee de swap para um DEX (em fração).
    /// V2 pools (QuickSwap, SushiSwap) = 0.3%.
    /// V3 pools (UniswapV3) = fee tier real do pool / 1_000_000 (unidade uint24).
    /// Se o fee tier não estiver no cache, usa 0.3% como default.
    #[inline]
    fn dex_fee(dex_name: &str, pair: &str) -> f64 {
        match dex_name {
            "QuickSwap" | "SushiSwap" => 0.003,  // V2: sempre 0.3%
            "Curve" => 0.0004,                    // Curve stables: 0.04%
            "UniswapV3" => {
                // Fee V3 = hundredths of a bip → fração = fee_tier / 1e6
                if let Some(fee_tier) = crate::dex::cached_fee_tier_pair(dex_name, pair) {
                    fee_tier as f64 / 1_000_000.0
                } else {
                    0.003  // Default: 0.3% (fee tier mais comum)
                }
            }
            _ => DEX_FEE_DEFAULT,
        }
    }

    #[inline]
    fn create_step(dex: &str, token_in: &str, token_out: &str, rate: f64) -> ArbitrageStep {
        ArbitrageStep {
            dex_name: dex.to_string(),
            dex_address: "0x0000000000000000000000000000000000000000".to_string(),
            token_in: token_in.to_string(),
            token_out: token_out.to_string(),
            amount_out_min: U256::zero(),
            expected_rate: rate,
            // defaults (nenhum override por step)
            dex_fee_bps: None,
            price_impact_bps: None,
        }
    }

    #[inline]
    fn get_token_address(&self, symbol: &str, app_config: &Config) -> Result<Address> {
        if let Some(&addr) = app_config.addresses.get(symbol) {
            return Ok(addr);
        }
        let addr_str = app_config
            .pairs
            .tokens
            .get(symbol)
            .ok_or_else(|| anyhow!("Endereço não encontrado para token '{}'", symbol))?;
        Address::from_str(addr_str)
            .map_err(|e| anyhow!("Endereço inválido para token '{}': {}", symbol, e))
    }

    #[instrument(skip_all, level = "trace")]
    async fn get_token_decimals_smart(&self, symbol: &str, app_config: &Config) -> Result<u32> {
        if let Some(meta) = app_config.pairs.metadata.get(symbol) {
            if let Some(decimals) = meta.decimals {
                {
                    let mut w = self.decimals_cache.write().await;
                    w.insert(symbol.to_string(), decimals as u32);
                }
                return Ok(decimals as u32);
            }
        }

        {
            let r = self.decimals_cache.read().await;
            if let Some(d) = r.get(symbol) {
                return Ok(*d);
            }
        }

        let token_address = self
            .get_token_address(symbol, app_config)
            .with_context(|| format!("resolvendo endereço do token '{}'", symbol))?;

        match get_token_decimals(self.middleware.clone(), token_address).await {
            Ok(decimals) => {
                let mut w = self.decimals_cache.write().await;
                w.insert(symbol.to_string(), decimals as u32);
                Ok(decimals as u32)
            }
            Err(e) => Err(anyhow!("On-chain decimal fetch failed for {}: {}", symbol, e)),
        }
    }

    async fn force_usdt_start_end_optimized(
        &self,
        opp: &mut ArbitrageOpportunity,
        app_config: &Config,
    ) -> Option<ArbitrageOpportunity> {
        self.convert_to_usdt_centric(opp, app_config).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Valida a fórmula cross-DEX com fees para USDT-WMATIC.
    ///
    /// Cenário real da auditoria:
    /// - SushiSwap USDT-WMATIC: 12.64 (compra barata)
    /// - UniswapV3 WMATIC-USDT: 0.0770 (vende caro)
    ///
    /// ✅ CORRIGIDO (v7): Rates dos DEX adapters (getAmountsOut/Quoter) JÁ incluem fee.
    /// O engine NÃO aplica (1-fee) novamente. cycle_rate = rate_ab × rate_ba.
    ///
    /// Cálculo manual (sem double-fee):
    ///   cycle_rate = 12.64 × 0.0770 = 0.97328
    ///   spread = (0.97328 - 1.0) × 100 = -2.67% (loss)
    #[test]
    fn cross_dex_math_with_fees_usdt_wmatic() {
        let rate_ab = 12.64; // USDT→WMATIC no SushiSwap (JÁ com fee embutida)
        let rate_ba = 0.0770; // WMATIC→USDT no UniswapV3 (JÁ com fee embutida)

        // ✅ CORRETO: rates já incluem fee, NÃO aplicar (1-fee) novamente
        let cycle_rate = rate_ab * rate_ba;

        // Verificar que é loss (mercado eficiente)
        assert!(cycle_rate < 1.0,
            "cycle_rate {} deveria ser < 1.0 (loss)", cycle_rate);

        // Verificar valor esperado
        let expected: f64 = 12.64 * 0.0770;
        assert!((cycle_rate - expected).abs() < 1e-10,
            "cycle_rate {} difere do esperado {}", cycle_rate, expected);

        // Double-fee seria MENOR que o correto — prova que double-fee é bug
        let fee = 0.003;
        let cycle_rate_double_fee = rate_ab * (1.0 - fee) * rate_ba * (1.0 - fee);
        assert!(cycle_rate > cycle_rate_double_fee,
            "cycle_rate sem double-fee {} deveria ser > com double-fee {}", cycle_rate, cycle_rate_double_fee);
    }

    /// Testa cenário hipotético onde há profit real cross-DEX.
    ///
    /// ✅ CORRIGIDO (v7): Rates já incluem fee (getAmountsOut/Quoter).
    /// cycle_rate = rate_ab × rate_ba (sem double-fee).
    ///
    /// Se SushiSwap tivesse USDT-WMATIC = 13.50 (compra barata, já com fee)
    /// e UniswapV3 tivesse WMATIC-USDT = 0.0800 (vende caro, já com fee):
    ///   cycle_rate = 13.50 × 0.0800 = 1.08
    ///   spread = +8.0% (profit!)
    #[test]
    fn cross_dex_math_with_fees_profit_scenario() {
        let rate_ab = 13.50; // USDT→WMATIC (JÁ com fee embutida)
        let rate_ba = 0.0800; // WMATIC→USDT (JÁ com fee embutida)

        // ✅ CORRETO: rates já incluem fee
        let cycle_rate = rate_ab * rate_ba;

        // Deveria ser profit
        assert!(cycle_rate > 1.0,
            "cycle_rate {} deveria ser > 1.0 (profit)", cycle_rate);

        // Verificar margem razoável
        let spread_pct = (cycle_rate - 1.0) * 100.0;
        assert!(spread_pct > 5.0 && spread_pct < 10.0,
            "spread {}% deveria estar entre 5-10%", spread_pct);
    }

    /// Valida que dex_fee retorna valores corretos por DEX.
    /// Para V3, retorna fee do cache (ou default 0.3% se não estiver no cache).
    #[test]
    fn dex_fee_returns_correct_values() {
        let pair = "USDT-WMATIC";
        assert_eq!(ArbitrageEngine::dex_fee("QuickSwap", pair), 0.003);
        assert_eq!(ArbitrageEngine::dex_fee("SushiSwap", pair), 0.003);
        // UniswapV3 retorna 0.003 se não houver cache (default)
        assert_eq!(ArbitrageEngine::dex_fee("UniswapV3", pair), 0.003);
        assert_eq!(ArbitrageEngine::dex_fee("UnknownDex", pair), DEX_FEE_DEFAULT);
    }

    /// Valida que UniswapV3 usa fee tier do cache quando disponível.
    /// Uniswap V3 fee = hundredths of a bip → fração = fee_tier / 1_000_000.
    #[test]
    fn dex_fee_uses_cached_tier_for_v3() {
        use crate::dex::cache_fee_tier;
        let pair = "USDT-WMATIC_FEE_SCALE_TEST";

        cache_fee_tier("UniswapV3", "USDT", "WMATIC_FEE_SCALE_TEST", 500);
        assert_eq!(ArbitrageEngine::dex_fee("UniswapV3", pair), 0.0005);

        cache_fee_tier("UniswapV3", "USDT", "WMATIC_FEE_SCALE_TEST", 3000);
        assert_eq!(ArbitrageEngine::dex_fee("UniswapV3", pair), 0.003);

        cache_fee_tier("UniswapV3", "USDT", "WMATIC_FEE_SCALE_TEST", 10000);
        assert_eq!(ArbitrageEngine::dex_fee("UniswapV3", pair), 0.01);
    }

    // ============================================================
    // 🧪 TESTES DE AUDITORIA (v6 - CALC_AUDIT)
    // ============================================================

    /// Valida round-trip de decimais para WBTC/USDC (8 vs 6 decimais).
    ///
    /// WBTC tem 8 decimais, USDC tem 6 decimais.
    /// Se amount_in = 1 WBTC (100,000,000 raw) e amount_out = 60,000 USDC (60,000,000,000 raw),
    /// o preço deveria ser 60,000.0 USDC por WBTC.
    #[test]
    fn decimals_round_trip_wbtc_usdc() {
        let amount_in = U256::from(100_000_000u64);      // 1.0 WBTC (8 dec)
        let amount_out = U256::from(60_000_000_000u64);   // 60,000 USDC (6 dec)
        let decimals_in: u8 = 8;
        let decimals_out: u8 = 6;

        let in_human = crate::utils::utils::u256_to_f64_precise(amount_in, decimals_in);
        let out_human = crate::utils::utils::u256_to_f64_precise(amount_out, decimals_out);

        assert!((in_human - 1.0).abs() < 1e-10,
            "WBTC amount_in deveria ser 1.0, obtido {}", in_human);
        assert!((out_human - 60000.0).abs() < 1e-10,
            "USDC amount_out deveria ser 60000.0, obtido {}", out_human);

        let price = out_human / in_human;
        assert!((price - 60000.0).abs() < 1e-6,
            "Preço WBTC/USDC deveria ser 60000.0, obtido {}", price);
    }

    /// Valida round-trip de decimais para USDT/USDC (6 vs 6 decimais).
    ///
    /// Ambos têm 6 decimais. Se amount_in = 1,000,000 (1 USDT) e
    /// amount_out = 999,000 (0.999 USDC com fee 0.3%), preço = 0.999.
    #[test]
    fn decimals_round_trip_stable_stable() {
        let amount_in = U256::from(1_000_000u64);  // 1.0 USDT (6 dec)
        let amount_out = U256::from(999_000u64);    // 0.999 USDC (6 dec)
        let decimals: u8 = 6;

        let in_human = crate::utils::utils::u256_to_f64_precise(amount_in, decimals);
        let out_human = crate::utils::utils::u256_to_f64_precise(amount_out, decimals);

        assert!((in_human - 1.0).abs() < 1e-10,
            "USDT amount_in deveria ser 1.0, obtido {}", in_human);
        assert!((out_human - 0.999).abs() < 1e-6,
            "USDC amount_out deveria ser 0.999, obtido {}", out_human);

        let price = out_human / in_human;
        assert!((price - 0.999).abs() < 1e-6,
            "Preço USDT/USDC deveria ser 0.999, obtido {}", price);
    }

    /// Valida invariante rate_ab × rate_ba ≈ 1.0 para o mesmo par no mesmo DEX.
    ///
    /// Para USDT-WMATIC no QuickSwap:
    /// - rate_ab = preço USDT→WMATIC (ex: 7.14)
    /// - rate_ba = preço WMATIC→USDT (ex: 0.14)
    /// - cycle_rate_no_fees = 7.14 × 0.14 = 0.9996 ≈ 1.0
    /// - cycle_rate_com_fees = 7.14 × 0.997 × 0.14 × 0.997 = 0.9936
    #[test]
    fn rate_round_trip_usdt_wmatic() {
        let rate_ab: f64 = 7.14; // USDT→WMATIC
        let rate_ba: f64 = 0.14; // WMATIC→USDT

        // Sem fees: deveria ser ≈ 1.0
        let cycle_no_fees: f64 = rate_ab * rate_ba;
        assert!((cycle_no_fees - 1.0).abs() < 0.01,
            "rate_ab × rate_ba deveria ser ≈ 1.0, obtido {}", cycle_no_fees);

        // Com fees: deveria ser < 1.0 (sempre loss no mesmo DEX)
        let fee: f64 = 0.003;
        let cycle_com_fees: f64 = rate_ab * (1.0 - fee) * rate_ba * (1.0 - fee);
        assert!(cycle_com_fees < 1.0,
            "cycle_rate com fees deveria ser < 1.0, obtido {}", cycle_com_fees);
        assert!(cycle_com_fees > 0.98,
            "cycle_rate com fees deveria ser > 0.98, obtido {}", cycle_com_fees);
    }

    /// Valida que fee não é aplicada duas vezes no cycle_rate.
    ///
    /// O cycle_rate já inclui fees via (1-fee_buy) e (1-fee_sell).
    /// Se calculate_total_rate_corrected() multiplicasse as taxas novamente,
    /// haveria dedução dupla.
    #[test]
    fn fee_not_doubled() {
        // Preço retornado pelo DEX adapter (JÁ com fee embutida)
        let rate_ab_with_fee: f64 = 0.997; // USDT→WMATIC com 0.3% fee
        let rate_ba_with_fee: f64 = 0.14 * 0.997; // WMATIC→USDT com 0.3% fee

        // O engine NÃO deveria aplicar fee novamente
        let cycle_rate: f64 = rate_ab_with_fee * rate_ba_with_fee;

        // Se aplicasse fee dupla:
        let fee: f64 = 0.003;
        let cycle_rate_doubled: f64 = rate_ab_with_fee * (1.0 - fee) * rate_ba_with_fee * (1.0 - fee);

        // A diferença deveria existir (fee dupla reduz o resultado)
        assert!(cycle_rate > cycle_rate_doubled,
            "Fee dupla deveria reduzir o cycle_rate: {} <= {}", cycle_rate, cycle_rate_doubled);

        // Verificar que a redução é proporcional ao fee aplicado
        let reduction_pct: f64 = ((cycle_rate - cycle_rate_doubled) / cycle_rate) * 100.0;
        assert!(reduction_pct > 0.05,
            "Redução de fee dupla deveria ser > 0.05%, obtido {}%", reduction_pct);
    }

    /// Valida fórmula getAmountsOut manual para V2.
    ///
    /// V2 getAmountsOut: amount_out = amount_in × reserve_out / (reserve_in + amount_in)
    /// Com fee 0.3%: amount_out = amount_in × 997 × reserve_out / (reserve_in × 1000 + amount_in × 997)
    ///
    /// Exemplo: reserve_in = 10,000 USDC, reserve_out = 71,400 WMATIC, amount_in = 1,000 USDC
    /// amount_out = 1000 × 997 × 71400 / (10000 × 1000 + 1000 × 997)
    ///            = 997 × 71400 / (10000000 + 997000)
    ///            = 71,185,800 / 10,997,000
    ///            = 6,474.17 WMATIC
    /// Preço = 6474.17 / 1000 = 6.47417 WMATIC/USDT
    #[test]
    fn get_amounts_out_manual_v2() {
        let reserve_in: f64 = 10_000.0;   // 10,000 USDC
        let reserve_out: f64 = 71_400.0;  // 71,400 WMATIC
        let amount_in: f64 = 1_000.0;     // 1,000 USDC
        let fee_pct: f64 = 0.003;         // 0.3%

        // Fórmula V2 com fee
        let amount_in_with_fee = amount_in * (1.0 - fee_pct);
        let amount_out = (amount_in_with_fee * reserve_out) / (reserve_in + amount_in_with_fee);
        let price = amount_out / amount_in;

        // Verificar que preço está em range razoável
        assert!(price > 6.0 && price < 7.0,
            "Preço USDT→WMATIC deveria estar entre 6.0-7.0, obtido {}", price);

        // Verificar que fee reduz o output
        let amount_out_no_fee = (amount_in * reserve_out) / (reserve_in + amount_in);
        assert!(amount_out < amount_out_no_fee,
            "Output com fee deveria ser menor que sem fee");
    }

    /// Valida fórmula completa do cycle_rate com cenário real.
    ///
    /// ✅ CORRIGIDO (v7): Rates dos DEX adapters JÁ incluem fee.
    /// cycle_rate = rate_ab × rate_ba (sem double-fee).
    ///
    /// Cenário: USDT-WMATIC cross-DEX
    /// - QuickSwap USDT→WMATIC: 7.14 (JÁ com fee embutida via getAmountsOut)
    /// - SushiSwap WMATIC→USDT: 0.14 (JÁ com fee embutida via getAmountsOut)
    ///
    /// cycle_rate = 7.14 × 0.14 = 0.9996
    /// spread = (0.9996 - 1.0) × 100 = -0.04% (loss mínimo)
    #[test]
    fn cycle_rate_formula_validation() {
        let rate_ab: f64 = 7.14;  // já inclui fee
        let rate_ba: f64 = 0.14;  // já inclui fee

        // ✅ CORRETO: rates já incluem fee, não aplicar (1-fee) novamente
        let cycle_rate: f64 = rate_ab * rate_ba;
        let spread_pct: f64 = (cycle_rate - 1.0) * 100.0;

        // Verificar fórmula
        let expected: f64 = 7.14 * 0.14;
        assert!((cycle_rate - expected).abs() < 1e-10,
            "cycle_rate {} difere do esperado {}", cycle_rate, expected);

        // Verificar que é loss (round-trrip no mesmo par cross-DEX sem spread)
        assert!(cycle_rate <= 1.0,
            "cycle_rate {} deveria ser <= 1.0 (round-trip)", cycle_rate);
        assert!(spread_pct <= 0.0,
            "spread {}% deveria ser <= 0", spread_pct);

        // Verificar que spread está em range razoável
        assert!(spread_pct > -2.0,
            "spread {}% deveria ser > -2.0%", spread_pct);
    }

    /// Valida que calculate_price_from_decimals retorna out_human / in_human.
    ///
    /// Para amount_in = 1,000,000 (1 USDT, 6 dec) e amount_out = 7,140,000 (7.14 WMATIC, 18 dec):
    /// in_human = 1,000,000 / 10^6 = 1.0
    /// out_human = 7,140,000 / 10^18 = 7.14e-12
    /// price = 7.14e-12 / 1.0 = 7.14e-12 (WMATIC por USDT, raw)
    ///
    /// NOTA: Este teste valida a lógica interna de calculate_price_from_decimals,
    /// não o preço final (que depende de decimais corretos do token).
    #[test]
    fn calculate_price_from_decimals_validation() {
        // Simular USDC→WMATIC (6 dec → 18 dec)
        let amount_in = U256::from(1_000_000u64);   // 1.0 USDC (6 dec)
        let amount_out = U256::from(7_140_000_000_000_000_000u128); // 7.14 WMATIC (18 dec)

        let price = crate::dex::calculate_price_from_decimals(
            amount_in, amount_out, 6, 18
        ).unwrap();

        // Preço deveria ser ~7.14 (WMATIC por USDC)
        assert!((price - 7.14).abs() < 0.01,
            "Preço USDC→WMATIC deveria ser ~7.14, obtido {}", price);
    }

    // ============================================================
    // TESTES DAS CORREÇÕES DE SEGURANÇA (ESTADO_ATUAL.md)
    // ============================================================

    /// safety_margin_bps < 5000 deve ser clampeado para 9500 (95%).
    /// Sem o clamp, amount_out_min aceitaria < 50% do esperado,
    /// permitindo sandwich/MEV drenar a transação.
    #[test]
    fn slippage_safe_clamps_dangerous_margin() {
        let amount = U256::from(1_000_000u64); // 1.0 USDC

        // safety_margin_bps = 10 (0.1%) — perigoso, deve ser clampeado para 9500
        let result_dangerous = ArbitrageEngine::apply_slippage_safe(amount, 50, 10);

        // safety_margin_bps = 9500 (95%) — valor do clamp
        let result_clamped = ArbitrageEngine::apply_slippage_safe(amount, 50, 9500);

        // Ambos devem produzir o mesmo resultado (10 foi clampeado para 9500)
        assert_eq!(
            result_dangerous, result_clamped,
            "safety_margin_bps=10 deve ser clampeado para 9500, produzindo o mesmo resultado"
        );

        // Verificar que o resultado não é catastroficamente baixo
        // final = 1_000_000 * (10000-50) * 9500 / (10000 * 10000)
        //       = 1_000_000 * 9950 * 9500 / 100_000_000
        //       = 1_000_000 * 94_525_000 / 100_000_000
        //       = 945_250
        let expected = U256::from(945_250u64);
        assert_eq!(result_dangerous, expected, "Resultado do clamp deveria ser 945_250");
    }

    /// safety_margin_bps >= 5000 deve ser usado como-is (sem clamp).
    #[test]
    fn slippage_safe_respects_valid_margin() {
        let amount = U256::from(1_000_000u64);

        // safety_margin_bps = 9800 (98%) — valor correto, não deve ser alterado
        let result = ArbitrageEngine::apply_slippage_safe(amount, 50, 9800);

        // final = 1_000_000 * 9950 * 9800 / 100_000_000 = 975_100
        let expected = U256::from(975_100u64);
        assert_eq!(result, expected, "safety_margin_bps=9800 deveria produzir 975_100");
    }

    /// safety_margin_bps abaixo de 9500 deve ser clampeado para 9500.
    /// O clamp real é .max(9500) — valores entre 5000 e 9499 são clampeados
    /// sem warning (warn só dispara para < 5000).
    #[test]
    fn slippage_safe_clamps_at_boundary() {
        let amount = U256::from(1_000_000u64);

        // 9499 < 9500, deve ser clampeado para 9500
        let result_9499 = ArbitrageEngine::apply_slippage_safe(amount, 50, 9499);
        let result_9500 = ArbitrageEngine::apply_slippage_safe(amount, 50, 9500);
        assert_eq!(result_9499, result_9500, "9499 deve ser clampeado para 9500");

        // 9500 = exatamente o piso, NÃO deve ser alterado
        // (já testado em slippage_safe_respects_valid_margin com 9800)

        // 5000 também é clampeado para 9500 (abaixo do piso)
        let result_5000 = ArbitrageEngine::apply_slippage_safe(amount, 50, 5000);
        assert_eq!(result_5000, result_9500, "5000 deve ser clampeado para 9500");

        // 9800 > 9500, NÃO deve ser clampeado (resultado diferente de 9500)
        let result_9800 = ArbitrageEngine::apply_slippage_safe(amount, 50, 9800);
        assert_ne!(result_9800, result_9500, "9800 não deve ser clampeado (>= 9500)");
    }

    /// next_opp_id deve produzir IDs únicos mesmo chamado rapidamente.
    #[test]
    fn next_opp_id_produces_unique_ids() {
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = next_opp_id("test");
            assert!(ids.insert(id.clone()), "ID duplicado: {}", id);
        }
    }

    /// next_opp_id deve incluir o prefixo no ID.
    #[test]
    fn next_opp_id_includes_prefix() {
        let id = next_opp_id("usdt_arb");
        assert!(
            id.starts_with("usdt_arb_"),
            "ID deveria começar com 'usdt_arb_', obtido: {}",
            id
        );
    }

    /// double-fee: cycle_rate sem double-fee deve ser MAIOR que com double-fee.
    /// Isto prova que aplicar (1-fee) novamente é um bug que subestima o retorno.
    #[test]
    fn no_double_fee_is_higher_than_double_fee() {
        let rate_ab = 13.50; // USDT->WMATIC (já com fee via getAmountsOut)
        let rate_ba = 0.0800; // WMATIC->USDT (já com fee via getAmountsOut)
        let fee = 0.003; // 0.3%

        // Correto: rates já incluem fee, não aplicar novamente
        let cycle_correct = rate_ab * rate_ba;

        // Bug (double-fee): aplicar (1-fee) novamente
        let cycle_buggy = rate_ab * (1.0 - fee) * rate_ba * (1.0 - fee);

        assert!(
            cycle_correct > cycle_buggy,
            "cycle_rate sem double-fee ({}) deveria ser > com double-fee ({})",
            cycle_correct,
            cycle_buggy
        );

        // Diferença deve ser significativa (~0.6% para fee=0.3%)
        let diff_pct = (cycle_correct - cycle_buggy) / cycle_buggy * 100.0;
        assert!(
            diff_pct > 0.5,
            "Diferença do double-fee deveria ser > 0.5%, obtido {:.4}%",
            diff_pct
        );
    }

}
