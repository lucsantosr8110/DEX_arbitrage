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
const MAX_REALISTIC_SPREAD: f64 = 100.0;
const MAX_REALISTIC_PROFIT_RATIO: f64 = 0.50;

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
        debug!("📊 ANALYSING PRICE_MAP - DEX Count: {}", price_map.len());

        for (dex, pairs) in price_map {
            debug!("📊 DEX '{}': {} pares", dex, pairs.len());
            for (pair, price) in pairs.iter().take(3) {
                debug!("    {} = {}", pair, price);
            }
            if pairs.len() > 3 {
                debug!("    ... e mais {} pares", pairs.len() - 3);
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

        let opportunity_id = format!("usdt_arb_{}", Utc::now().timestamp_millis());
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

                // Fórmula cross-DEX com fees:
                // amount_out = amount_in × rate_ab × (1 - fee_buy) × rate_ba × (1 - fee_sell)
                // cycle_rate inclui o impacto das fees de AMBOS os DEXes
                let fee_buy = Self::dex_fee(buy_dex);
                let fee_sell = Self::dex_fee(sell_dex);
                let cycle_rate = rate_ab * (1.0 - fee_buy) * rate_ba * (1.0 - fee_sell);
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
            info!("  ❌ {}: nenhum cycle cross-DEX viável (rates_ab={:?}, rates_ba={:?})", pair,
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
            id: format!("direct_arb_{}", Utc::now().timestamp_millis()),
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
        let opportunity_id = format!("usdt_tri_{}", Utc::now().timestamp_millis());

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
        let base_amount = if app_config.flashloan.enabled {
            app_config.flashloan.capital_usd
        } else {
            app_config
                .arbitrage
                .default_trade_amount
                .parse::<f64>()
                .unwrap_or(1.0)
        };

        base_amount.clamp(MIN_TRADE_AMOUNT_USD, MAX_TRADE_AMOUNT_USD)
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
    fn apply_slippage_safe(amount: U256, slippage_bps: u32, safety_margin_bps: u32) -> U256 {
        let bps = U256::from(10_000u64);
        let sl_bps = U256::from(slippage_bps as u64);
        let safety_bps = U256::from(safety_margin_bps as u64);

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
    /// V3 pools (UniswapV3) = 0.3% default (pools reais variam 0.01%-1%).
    #[inline]
    fn dex_fee(dex_name: &str) -> f64 {
        match dex_name {
            "QuickSwap" | "SushiSwap" => 0.003,  // V2: 0.3%
            "UniswapV3" => 0.003,                 // V3 default: 0.3%
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
    /// - Fees: 0.3% cada DEX
    ///
    /// Cálculo manual:
    ///   amount_out = 100 × 12.64 × (1-0.003) × 0.0770 × (1-0.003)
    ///             = 100 × 12.64 × 0.997 × 0.0770 × 0.997
    ///             = 100 × 12.60108 × 0.0770 × 0.997
    ///             = 100 × 0.970283 × 0.997
    ///             = 96.737
    ///   cycle_rate = 96.737 / 100 = 0.96737
    ///   spread = (0.96737 - 1.0) × 100 = -3.26% (loss)
    #[test]
    fn cross_dex_math_with_fees_usdt_wmatic() {
        let rate_ab = 12.64; // USDT→WMATIC no SushiSwap (compra barata)
        let rate_ba = 0.0770; // WMATIC→USDT no UniswapV3 (vende caro)
        let fee_buy = 0.003; // 0.3% V2
        let fee_sell = 0.003; // 0.3% V3 default

        let cycle_rate = rate_ab * (1.0 - fee_buy) * rate_ba * (1.0 - fee_sell);

        // Verificar que o cycle_rate está correto
        let expected: f64 = 12.64 * 0.997 * 0.0770 * 0.997;
        assert!((cycle_rate - expected).abs() < 1e-10,
            "cycle_rate {} difere do esperado {}", cycle_rate, expected);

        // Verificar que é loss (mercado eficiente)
        assert!(cycle_rate < 1.0,
            "cycle_rate {} deveria ser < 1.0 (loss)", cycle_rate);

        // Verificar que sem fees seria profit
        let cycle_rate_no_fees = rate_ab * rate_ba;
        assert!(cycle_rate_no_fees < 1.0,
            "mesmo sem fees, cycle_rate {} deveria ser < 1.0", cycle_rate_no_fees);
    }

    /// Testa cenário hipotético onde há profit real cross-DEX.
    ///
    /// Se SushiSwap tivesse USDT-WMATIC = 13.50 (compra barata)
    /// e UniswapV3 tivesse WMATIC-USDT = 0.0800 (vende caro):
    ///   cycle_rate = 13.50 × 0.997 × 0.0800 × 0.997 = 1.0733
    ///   spread = +7.33% (profit!)
    #[test]
    fn cross_dex_math_with_fees_profit_scenario() {
        let rate_ab = 13.50; // USDT→WMATIC mais barato
        let rate_ba = 0.0800; // WMATIC→USDT mais caro
        let fee_buy = 0.003;
        let fee_sell = 0.003;

        let cycle_rate = rate_ab * (1.0 - fee_buy) * rate_ba * (1.0 - fee_sell);

        // Deveria ser profit
        assert!(cycle_rate > 1.0,
            "cycle_rate {} deveria ser > 1.0 (profit)", cycle_rate);

        // Verificar margem razoável
        let spread_pct = (cycle_rate - 1.0) * 100.0;
        assert!(spread_pct > 5.0 && spread_pct < 10.0,
            "spread {}% deveria estar entre 5-10%", spread_pct);
    }

    /// Valida que dex_fee retorna valores corretos por DEX.
    #[test]
    fn dex_fee_returns_correct_values() {
        assert_eq!(ArbitrageEngine::dex_fee("QuickSwap"), 0.003);
        assert_eq!(ArbitrageEngine::dex_fee("SushiSwap"), 0.003);
        assert_eq!(ArbitrageEngine::dex_fee("UniswapV3"), 0.003);
        assert_eq!(ArbitrageEngine::dex_fee("UnknownDex"), DEX_FEE_DEFAULT);
    }
}
