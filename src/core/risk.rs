// ============================================================
// src/core/risk.rs — VERSÃO COMPATÍVEL COM ARBITRAGE.RS
// ✅ Totalmente compatível com a estrutura do arbitrage.rs
// ✅ Usa os mesmos tipos e estruturas
// ✅ Totalmente parametrizado por config.toml
// ============================================================

use crate::core::types::{
    ArbitrageOpportunity, FlashloanOpportunity, RiskAssessment, RiskConfig, RiskFactor,
};
use crate::infra::metrics;
use crate::utils::validate_price;
use once_cell::sync::OnceCell;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info};

// ============================================================
// 🔒 RISK_MANAGER Global
// ============================================================

pub static RISK_MANAGER: OnceCell<Arc<Mutex<RiskManager>>> = OnceCell::new();

pub fn init_global_risk_manager(cfg: RiskConfig) -> bool {
    RISK_MANAGER.set(Arc::new(Mutex::new(RiskManager::new(cfg)))).is_ok()
}

pub fn get_global_risk_manager() -> Option<&'static Arc<Mutex<RiskManager>>> {
    RISK_MANAGER.get()
}

// ============================================================
// ⚙️ Estrutura principal do RiskManager
// ============================================================
pub struct RiskManager {
    pub config: RiskConfig,
    pub adaptive_mode: AtomicBool,
    pub last_adjustment: Instant,
    pub opportunities_analyzed: u64,
    pub opportunities_approved: u64,
}

impl RiskManager {
    pub fn new(config: RiskConfig) -> Self {
        Self {
            config,
            adaptive_mode: AtomicBool::new(false),
            last_adjustment: Instant::now(),
            opportunities_analyzed: 0,
            opportunities_approved: 0,
        }
    }

    pub fn with_defaults() -> Self {
        Self::new(RiskConfig::default())
    }

    // ============================================================
    // 🔧 Utilitários de cálculo — Gas / Slippage / Profit
    // ============================================================
    fn calculate_realistic_gas_usd(&self, adaptive: bool) -> f64 {
        let gas_min = if adaptive { 
            self.config.gas_min_usd_adaptive 
        } else { 
            self.config.gas_min_usd 
        };
        let gas_max = if adaptive { 
            self.config.gas_max_usd_adaptive 
        } else { 
            self.config.gas_max_usd 
        };
        let base_gas = (gas_min + gas_max) / 2.0;
        let multiplier = if adaptive { 
            self.config.gas_multiplier_adaptive 
        } else { 
            self.config.gas_multiplier 
        };
        (base_gas * multiplier).max(gas_min).min(gas_max)
    }

    fn calculate_realistic_slippage(&self, spread_percent: f64, adaptive: bool) -> f64 {
        let (coef, min_abs, max_abs) = if adaptive {
            (
                self.config.slippage_coef_adaptive,
                self.config.slippage_min_abs_adaptive,
                self.config.slippage_max_abs_adaptive,
            )
        } else {
            (
                self.config.slippage_coef_aggressive,
                self.config.slippage_min_abs_aggressive,
                self.config.slippage_max_abs_aggressive,
            )
        };
        let slip = spread_percent * coef;
        slip.max(min_abs).min(max_abs)
    }

    fn min_profit_threshold(&self, _adaptive: bool) -> f64 {
        // ✅ Totalmente definido via config.toml
        self.config.min_profit_usd
    }

    // ============================================================
    // 🎯 Avaliação principal — chama modo adaptativo ou agressivo
    // ============================================================
    pub fn assess_opportunity(&mut self, opportunity: &ArbitrageOpportunity) -> RiskAssessment {
        self.opportunities_analyzed += 1;

        if !self.validate_opportunity_basics(opportunity) {
            return RiskAssessment {
                approved: false,
                risk_factors: vec![RiskFactor::NegativeProfit],
                risk_score: 100.0,
                adaptive_mode: false,
            };
        }

        let assessment = if self.adaptive_mode.load(Ordering::Relaxed) {
            self.assess_opportunity_adaptive(opportunity)
        } else {
            self.assess_opportunity_aggressive(opportunity)
        };

        if assessment.approved {
            self.opportunities_approved += 1;
        }

        self.update_adaptive_mode();
        assessment
    }

    fn validate_opportunity_basics(&self, opportunity: &ArbitrageOpportunity) -> bool {
        // ✅ VALIDAÇÃO COMPATÍVEL COM ARBITRAGE.RS
        validate_price(opportunity.buy_price)
            && validate_price(opportunity.sell_price)
            && !opportunity.buy_dex.is_empty()
            && !opportunity.sell_dex.is_empty()
            && opportunity.buy_dex != opportunity.sell_dex
            && opportunity.estimated_profit_usd > 0.0
            && opportunity.spread_percent > 0.0
            && opportunity.confidence > 0.0
    }

    // ============================================================
    // 💡 Avaliação agressiva — otimizada para micro-lucros
    // ============================================================
    fn assess_opportunity_aggressive(&self, opportunity: &ArbitrageOpportunity) -> RiskAssessment {
        let mut risk_factors = Vec::new();
        let mut risk_score = 0.0;

        let gas_real = self.calculate_realistic_gas_usd(false);
        metrics::set_last_gas_usd(gas_real);

        let gross = opportunity.estimated_profit_usd.max(0.0);
        let slip = self.calculate_realistic_slippage(opportunity.spread_percent, false);
        let premium = self.config.premium_usd_aggressive.max(0.0);
        let adjusted_net_profit = gross - gas_real - premium - slip;
        let min_profit_usd = self.min_profit_threshold(false);

        debug!(
            "🧮 [Agg] gross=${:.6} gas=${:.6} prem=${:.6} slip=${:.6} net=${:.6} | min=${:.6} | conf={:.3}",
            gross, gas_real, premium, slip, adjusted_net_profit, min_profit_usd, opportunity.confidence
        );

        // ✅ VALIDAÇÃO DE LUCRO NEGATIVO - COMPATÍVEL
        if adjusted_net_profit < min_profit_usd {
            risk_factors.push(RiskFactor::NegativeProfit);
            risk_score += self.config.weight_negative_profit;
        }

        // ✅ VALIDAÇÃO DE PROFIT PERCENT - COMPATÍVEL
        let min_profit_percent = if opportunity.spread_percent < 1.0 {
            self.config.normal_min_profit_percent * 0.1 // mais tolerante para micro-spreads
        } else {
            self.config.normal_min_profit_percent
        };

        if opportunity.profit_percent < min_profit_percent {
            risk_factors.push(RiskFactor::ProfitTooLow);
            risk_score += self.config.weight_profit_too_low;
        }

        // ✅ VALIDAÇÃO DE GAS RATIO - COMPATÍVEL
        let gas_ratio = if adjusted_net_profit > 0.0 {
            gas_real / adjusted_net_profit
        } else {
            f64::INFINITY
        };
        let max_gas_ratio = if opportunity.spread_percent < 0.5 {
            self.config.normal_gas_ratio * 2.0 // mais tolerante para micro-spreads
        } else {
            self.config.normal_gas_ratio
        };
        if gas_ratio > max_gas_ratio {
            risk_factors.push(RiskFactor::GasCostTooHigh);
            risk_score += self.config.weight_gas_too_high;
        }

        // ✅ VALIDAÇÃO DE CONFIANÇA - COMPATÍVEL
        let min_conf = if opportunity.spread_percent > 1.0 {
            self.config.min_confidence
        } else {
            self.config.min_confidence * 0.7 // mais tolerante para micro-spreads
        };
        if opportunity.confidence < min_conf {
            risk_factors.push(RiskFactor::LowConfidence);
            risk_score += self.config.weight_low_confidence;
        }

        // ✅ VALIDAÇÃO DE VOLUME - COMPATÍVEL
        let min_volume = if opportunity.estimated_profit_usd < self.config.min_profit_usd {
            self.config.min_volume_usd * 0.5 // mais tolerante para micro-lucros
        } else {
            self.config.min_volume_usd
        };
        if opportunity.estimated_volume_usd < min_volume {
            risk_factors.push(RiskFactor::LowVolume);
            risk_score += self.config.weight_low_volume;
        }

        // ✅ VALIDAÇÃO DE EXECUTION RISK - COMPATÍVEL
        if opportunity.execution_risk > self.config.max_execution_risk {
            risk_factors.push(RiskFactor::HighExecutionRisk);
            risk_score += self.config.weight_high_execution_risk;
        }

        let approved = adjusted_net_profit >= min_profit_usd && risk_score <= self.config.max_risk_score;

        if approved {
            info!(
                "✅ [Agg] APPROVED | spread={:.3}% | net=${:.6} | min=${:.6} | score={:.1}",
                opportunity.spread_percent, adjusted_net_profit, min_profit_usd, risk_score
            );
        } else {
            debug!(
                "🚫 [Agg] REJECTED | spread={:.3}% | net=${:.6} | min=${:.6} | score={:.1}",
                opportunity.spread_percent, adjusted_net_profit, min_profit_usd, risk_score
            );
        }

        self.update_metrics(risk_score, approved, false);
        RiskAssessment { approved, risk_factors, risk_score, adaptive_mode: false }
    }

    // ============================================================
    // 🤖 Avaliação adaptativa — mais tolerante para micro-lucros
    // ============================================================
    fn assess_opportunity_adaptive(&self, opportunity: &ArbitrageOpportunity) -> RiskAssessment {
        let mut risk_factors = Vec::new();
        let mut risk_score = 0.0;

        let gas_real = self.calculate_realistic_gas_usd(true);
        metrics::set_last_gas_usd(gas_real);

        let gross = opportunity.estimated_profit_usd.max(0.0);
        let slip = self.calculate_realistic_slippage(opportunity.spread_percent, true);
        let premium = self.config.premium_usd_adaptive.max(0.0);
        let adjusted_net_profit = gross - gas_real - premium - slip;
        let min_profit_usd = self.min_profit_threshold(true);

        debug!(
            "🧮 [Adapt] gross=${:.6} gas=${:.6} prem=${:.6} slip=${:.6} net=${:.6} | min=${:.6}",
            gross, gas_real, premium, slip, adjusted_net_profit, min_profit_usd
        );

        // ✅ VALIDAÇÕES ADAPTATIVAS MAIS TOLERANTES
        if adjusted_net_profit < min_profit_usd {
            risk_factors.push(RiskFactor::NegativeProfit);
            risk_score += self.config.weight_negative_profit_adaptive * 0.5;
        }
        
        if opportunity.profit_percent < (self.config.adaptive_min_profit_percent * 0.5) {
            risk_factors.push(RiskFactor::ProfitTooLow);
            risk_score += self.config.weight_profit_too_low_adaptive * 0.5;
        }
        
        let gas_ratio = if adjusted_net_profit > 0.0 { 
            gas_real / adjusted_net_profit 
        } else { 
            f64::INFINITY 
        };
        
        if gas_ratio > (self.config.adaptive_gas_ratio * 1.5) {
            risk_factors.push(RiskFactor::GasCostTooHigh);
            risk_score += self.config.weight_gas_too_high_adaptive * 0.5;
        }
        
        if opportunity.confidence < (self.config.min_confidence * self.config.adaptive_confidence_factor) {
            risk_factors.push(RiskFactor::LowConfidence);
            risk_score += self.config.weight_low_confidence_adaptive * 0.5;
        }
        
        if opportunity.estimated_volume_usd < (self.config.min_volume_usd * self.config.adaptive_volume_factor) {
            risk_factors.push(RiskFactor::LowVolume);
            risk_score += self.config.weight_low_volume_adaptive * 0.5;
        }
        
        if opportunity.execution_risk > self.config.max_execution_risk {
            risk_factors.push(RiskFactor::HighExecutionRisk);
            risk_score += self.config.weight_high_execution_risk_adaptive;
        }

        let approved = adjusted_net_profit >= (min_profit_usd * 0.8)
            && risk_score <= (self.config.max_risk_score * self.config.adaptive_score_relax_factor);

        if approved {
            info!(
                "✅ [Adapt] APPROVED | spread={:.3}% | net=${:.6} | score={:.1}",
                opportunity.spread_percent, adjusted_net_profit, risk_score
            );
        }

        self.update_metrics(risk_score, approved, true);
        RiskAssessment { approved, risk_factors, risk_score, adaptive_mode: true }
    }

    // ============================================================
    // ⚡ Avaliação Flashloan — penalidade ajustada
    // ============================================================
    pub fn assess_flashloan_opportunity(
        &mut self,
        opportunity: &FlashloanOpportunity,
    ) -> RiskAssessment {
        let mut assessment = self.assess_opportunity(&opportunity.base_opportunity);
        
        // ✅ PENALIDADE POR STEPS ADICIONAIS - COMPATÍVEL
        let extra_steps = opportunity.steps.len().saturating_sub(3) as f64;
        let penalty = (extra_steps * self.config.weight_step_penalty).max(0.0);
        assessment.risk_score = (assessment.risk_score + penalty).min(100.0);
        
        assessment.adaptive_mode = self.adaptive_mode.load(Ordering::Relaxed);
        
        let max_risk = if self.adaptive_mode.load(Ordering::Relaxed) {
            self.config.max_risk_score * 1.1
        } else {
            self.config.max_risk_score
        };
        
        let approved = assessment.risk_score <= max_risk;
        RiskAssessment { approved, ..assessment }
    }

    // ============================================================
    // 📊 Métricas e modo adaptativo
    // ============================================================
    fn update_metrics(&self, risk_score: f64, approved: bool, adaptive: bool) {
        metrics::set_risk_score(risk_score);
        if approved {
            metrics::inc_risk_approve();
            if adaptive {
                metrics::inc_adaptive_approve();
            }
        } else {
            metrics::inc_risk_reject();
        }
    }

    fn update_adaptive_mode(&mut self) {
        let now = Instant::now();
        if self.opportunities_analyzed % self.config.adaptive_check_every_ops as u64 == 0
            || now.duration_since(self.last_adjustment) > Duration::from_secs(self.config.adaptive_check_secs as u64)
        {
            let hit_rate = self.calculate_hit_rate();
            let should_activate = hit_rate < self.config.adaptive_activate_below_hitrate;

            if should_activate != self.adaptive_mode.load(Ordering::Relaxed) {
                self.adaptive_mode.store(should_activate, Ordering::Relaxed);
                self.last_adjustment = now;
                info!(
                    "🚀 Adaptive mode {} | HitRate: {:.1}% | Ops: {}/{}",
                    if should_activate { "ON" } else { "OFF" },
                    hit_rate * 100.0,
                    self.opportunities_approved,
                    self.opportunities_analyzed
                );
            }
            metrics::set_adaptive_mode(should_activate);
            metrics::set_hit_rate(hit_rate * 100.0);
        }
    }

    fn calculate_hit_rate(&self) -> f64 {
        if self.opportunities_analyzed > 0 {
            self.opportunities_approved as f64 / self.opportunities_analyzed as f64
        } else {
            0.0
        }
    }

    // ============================================================
    // 📈 Estatísticas para monitoramento
    // ============================================================
    pub fn get_stats(&self) -> RiskManagerStats {
        RiskManagerStats {
            opportunities_analyzed: self.opportunities_analyzed,
            opportunities_approved: self.opportunities_approved,
            hit_rate: self.calculate_hit_rate(),
            adaptive_mode: self.adaptive_mode.load(Ordering::Relaxed),
            config: self.config.clone(),
        }
    }
}

// ============================================================
// 🔹 Estatísticas e Display
// ============================================================
#[derive(Debug, Clone)]
pub struct RiskManagerStats {
    pub opportunities_analyzed: u64,
    pub opportunities_approved: u64,
    pub hit_rate: f64,
    pub adaptive_mode: bool,
    pub config: RiskConfig,
}

impl std::fmt::Display for RiskManagerStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "RiskManagerStats: Analyzed={}, Approved={}, HitRate={:.1}%, Adaptive={} | min_profit=${:.6}",
            self.opportunities_analyzed,
            self.opportunities_approved,
            self.hit_rate * 100.0,
            self.adaptive_mode,
            self.config.min_profit_usd
        )
    }
}

// ============================================================
// 🧪 Testes unitários — compatíveis com arbitrage.rs
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::U256;

    fn create_test_opportunity() -> ArbitrageOpportunity {
        ArbitrageOpportunity {
            id: "test_001".to_string(),
            pair: "USDT-USDC".to_string(),
            buy_dex: "QuickSwap".to_string(),
            sell_dex: "SushiSwap".to_string(),
            buy_price: 0.999,
            sell_price: 1.001,
            spread_percent: 0.2,
            amount_in: U256::from(100_000_000u64), // 100 USDT
            amount_out: U256::from(100_200_000u64), // 100.2 USDC
            estimated_profit_usd: 0.15,
            gas_cost_usd: 0.05,
            net_profit_usd: 0.10,
            steps: crate::core::types::SerializableSteps(vec![]),
            path: vec!["USDT".to_string(), "USDC".to_string(), "USDT".to_string()],
            timestamp: 1234567890,
            confidence: 0.75,
            estimated_volume_usd: 100.0,
            profit_percent: 0.1,
            execution_risk: 0.1,
            force_flashloan: false,
            token_price_usd: Some(1.0),
        }
    }

    #[test]
    fn test_opportunity_validation() {
        let manager = RiskManager::with_defaults();
        let op = create_test_opportunity();
        assert!(manager.validate_opportunity_basics(&op));
    }

    #[test]
    fn test_risk_assessment_aggressive() {
        let mut manager = RiskManager::with_defaults();
        let op = create_test_opportunity();
        let assessment = manager.assess_opportunity(&op);
        assert!(assessment.risk_score >= 0.0);
        assert!(assessment.risk_score <= 100.0);
    }

    #[test]
    fn test_adaptive_mode_switch() {
        let mut manager = RiskManager::with_defaults();
        
        // Forçar modo adaptativo
        manager.adaptive_mode.store(true, Ordering::Relaxed);
        
        let op = create_test_opportunity();
        let assessment = manager.assess_opportunity(&op);
        
        assert!(assessment.adaptive_mode);
    }
}