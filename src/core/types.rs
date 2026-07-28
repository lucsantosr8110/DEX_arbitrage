use ethers::types::{Address, Bytes, H256, U256};
use serde::{Deserialize, Serialize};
use std::fmt;

// ============================================================================
// 🧾 ExecutionOutcome — estado explícito de uma tentativa lógica de execução
// ============================================================================
//
// Uma tentativa lógica de arbitragem (uma oportunidade, uma decisão de envio)
// produz exatamente um destes estados. Estados distintos jamais colapsam em
// "skipped":
//   - Reverted        = tx minerada com status 0 (queimou gás). NUNCA skip.
//   - SameBlockRejected = bloqueada pelo anti-MEV (mesmo bloco). Distinto de
//     Reverted (nem chegou a ser minerada como arbitragem).
//   - TimeoutStuck    = tx broadcast mas sem receipt; nonce possivelmente
//     pendente. Reenvio só como replacement (mesmo nonce), nunca nonce novo.
//   - Dropped         = tx rejeitada antes de entrar na rede; nonce liberado.
//   - AbortedPreBroadcast = abortada antes de qualquer broadcast (gate, simulação).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ExecutionOutcome {
    ConfirmedProfit {
        tx_hash: H256,
        realized_profit_usd: f64,
        gas_used: U256,
    },
    ConfirmedLoss {
        tx_hash: H256,
        realized_loss_usd: f64,
        gas_used: U256,
    },
    Reverted {
        tx_hash: H256,
        reason: Option<String>,
        gas_used: Option<U256>,
    },
    SameBlockRejected {
        tx_hash: Option<H256>,
    },
    TimeoutStuck {
        nonce: U256,
        latest_tx_hash: Option<H256>,
    },
    Dropped {
        nonce: U256,
    },
    /// B4: tx broadcast mas o re-bump de gas (RBF underpriced) ultrapassou o teto
    /// `gas_ceiling_gwei` ou esgotou `max_replace_attempts`. A tx pendente foi
    /// abandonada (não substituída por gas maior) — nonce ainda pode estar na
    /// rede; o nonce reaper (B8) cuida da recuperação. Não conta como executada.
    Expired {
        nonce: U256,
        latest_tx_hash: Option<H256>,
        reason: String,
    },
    AbortedPreBroadcast {
        reason: String,
    },
}

impl ExecutionOutcome {
    /// True quando o estado NÃO representa uma execuição broadcast+minerada
    /// (portanto não deve contar como sucesso nem como execução realizada).
    pub fn is_executed_onchain(&self) -> bool {
        matches!(
            self,
            ExecutionOutcome::ConfirmedProfit { .. }
                | ExecutionOutcome::ConfirmedLoss { .. }
                | ExecutionOutcome::Reverted { .. }
        )
    }

    /// True quando a tx foi minerada com sucesso (status 1).
    pub fn is_confirmed(&self) -> bool {
        matches!(
            self,
            ExecutionOutcome::ConfirmedProfit { .. } | ExecutionOutcome::ConfirmedLoss { .. }
        )
    }

    /// Revert é execução on-chain falha — distinto de skip/abort.
    pub fn is_reverted(&self) -> bool {
        matches!(self, ExecutionOutcome::Reverted { .. })
    }
}

// ============================================================================
// 🔹 Arbitrage Step — passo real de rota
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArbitrageStep {
    pub dex_name: String,
    pub dex_address: String,
    pub token_in: String,
    pub token_out: String,
    pub expected_rate: f64,
    pub amount_out_min: U256,

    /// Opcional: fee da DEX para esse step em BPS (base 10000). Se None, usar default vindo do Config.
    #[serde(default)]
    pub dex_fee_bps: Option<u32>,

    /// Opcional: price impact estimado para esse step em BPS (base 10000). Se None, usar default do Config.
    #[serde(default)]
    pub price_impact_bps: Option<u32>,

    /// Fee tier Uniswap V3 (`uint24` nativo: 500/3000/10000). Preenchido do
    /// `FEE_TIER_CACHE` (mesma `fee_cache_key` do radar). `None` em V2/Curve
    /// ou se o cache missar — execução V3 deve abortar, não defaultar 3000.
    #[serde(default)]
    pub v3_fee_tier: Option<u32>,
}

// Container serializável
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct SerializableSteps(pub Vec<ArbitrageStep>);

// ============================================================================
// 🔥 Arbitrage Opportunity — estrutura central
// ============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ArbitrageOpportunity {
    pub id: String,
    pub pair: String,
    pub buy_dex: String,
    pub sell_dex: String,
    pub buy_price: f64,
    pub sell_price: f64,
    pub spread_percent: f64,
    pub amount_in: U256,
    pub amount_out: U256,
    pub estimated_profit_usd: f64,
    pub gas_cost_usd: f64,
    pub net_profit_usd: f64,
    pub steps: SerializableSteps,
    pub path: Vec<String>,
    pub timestamp: u64,
    pub confidence: f64,
    pub estimated_volume_usd: f64,
    pub profit_percent: f64,
    pub execution_risk: f64,

    #[serde(default)]
    pub force_flashloan: bool,

    #[serde(default)]
    pub token_price_usd: Option<f64>,
}

impl ArbitrageOpportunity {
    // ------------------------------------------------------------
    // 🟢 Validação geral
    // ------------------------------------------------------------
    pub fn is_valid(&self) -> bool {
        self.buy_price > 0.0
            && self.sell_price > 0.0
            && !self.buy_dex.is_empty()
            && !self.sell_dex.is_empty()
            && self.spread_percent > 0.0
    }

    // ------------------------------------------------------------
    // 📈 Profit % baseado no total real
    // ------------------------------------------------------------
    pub fn calculate_profit_percent(&self) -> f64 {
        if self.estimated_profit_usd <= 0.0 {
            return 0.0;
        }
        self.estimated_profit_usd / (self.estimated_volume_usd + self.gas_cost_usd.max(1e-9))
    }

    // ------------------------------------------------------------
    // 🔄 Atualiza percentuais, risco, volume
    // ------------------------------------------------------------
    pub fn update_calculated_fields(&mut self) {
        // Profit %
        self.profit_percent = self.calculate_profit_percent() * 100.0;

        // Volume USD (amount_in * token_price)
        if let Some(price) = self.token_price_usd {
            let amount_dec = crate::utils::u256_to_f64(self.amount_in, 18);
            self.estimated_volume_usd = amount_dec * price;
        }

        // Execução arriscada = baixa confiança × volume pequeno
        let volume_factor = if self.estimated_volume_usd < 500.0 {
            0.85
        } else if self.estimated_volume_usd < 5_000.0 {
            0.65
        } else {
            0.40
        };

        self.execution_risk = (1.0 - self.confidence).max(0.0) * volume_factor;

        // Limites
        self.profit_percent = self.profit_percent.clamp(0.0, 2000.0);
        self.confidence = self.confidence.clamp(0.0, 1.0);
        self.execution_risk = self.execution_risk.clamp(0.0, 1.0);
    }
}

// ============================================================================
// 🔥 Flashloan Opportunity (Polygon Aave V3 / Flashloan Wrapper)
// ============================================================================

#[derive(Debug, Clone)]
pub struct FlashloanOpportunity {
    pub base_opportunity: ArbitrageOpportunity,
    pub asset: Address,
    pub amount: U256,
    pub steps: Vec<FlashloanStep>,
    pub expected_profit: f64,
    pub premium_cost: f64,
    pub gas_overhead: u64,
}

#[derive(Debug, Clone)]
pub struct FlashloanStep {
    pub dex_type: u8, // 0=UniV3,1=Sushi,2=QuickSwap, etc
    pub token_in: Address,
    pub token_out: Address,
    pub amount_out_min: U256,
    pub extra_data: Bytes,
}

// ============================================================================
// ⚖️ Risco e Avaliação
// ============================================================================
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RiskAssessment {
    pub approved: bool,
    pub risk_factors: Vec<RiskFactor>,
    pub risk_score: f64,
    pub adaptive_mode: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RiskFactor {
    GasCostTooHigh,
    NegativeProfit,
    ProfitTooLow,
    LowConfidence,
    HighSlippage,
    LowVolume,
    HighExecutionRisk,
}

impl fmt::Display for RiskFactor {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use RiskFactor::*;
        let text = match self {
            GasCostTooHigh => "GasCostTooHigh",
            NegativeProfit => "NegativeProfit",
            ProfitTooLow => "ProfitTooLow",
            LowConfidence => "LowConfidence",
            HighSlippage => "HighSlippage",
            LowVolume => "LowVolume",
            HighExecutionRisk => "HighExecutionRisk",
        };
        write!(f, "{}", text)
    }
}

// ============================================================================
// 🧮 Configuração de Risco — versão otimizada para Polygon HFT
// ============================================================================
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct RiskConfig {
    pub min_profit_usd: f64,
    pub normal_min_profit_percent: f64,
    pub adaptive_min_profit_percent: f64,
    pub normal_gas_ratio: f64,
    pub adaptive_gas_ratio: f64,
    pub min_confidence: f64,
    pub min_volume_usd: f64,
    pub max_execution_risk: f64,
    pub max_risk_score: f64,
    pub default_token_decimals: u32,

    pub gas_dynamic_k: f64,
    pub gas_min_usd: f64,
    pub gas_max_usd: f64,
    pub gas_multiplier: f64,

    pub gas_dynamic_k_adaptive: f64,
    pub gas_min_usd_adaptive: f64,
    pub gas_max_usd_adaptive: f64,
    pub gas_multiplier_adaptive: f64,

    pub slippage_coef_aggressive: f64,
    pub slippage_min_abs_aggressive: f64,
    pub slippage_max_abs_aggressive: f64,

    pub slippage_coef_adaptive: f64,
    pub slippage_min_abs_adaptive: f64,
    pub slippage_max_abs_adaptive: f64,

    pub premium_usd_aggressive: f64,
    pub premium_usd_adaptive: f64,

    pub weight_negative_profit: f64,
    pub weight_profit_too_low: f64,
    pub weight_gas_too_high: f64,
    pub weight_low_confidence: f64,
    pub weight_low_volume: f64,
    pub weight_high_execution_risk: f64,

    pub weight_negative_profit_adaptive: f64,
    pub weight_profit_too_low_adaptive: f64,
    pub weight_gas_too_high_adaptive: f64,
    pub weight_low_confidence_adaptive: f64,
    pub weight_low_volume_adaptive: f64,
    pub weight_high_execution_risk_adaptive: f64,

    pub weight_step_penalty: f64,
    pub adaptive_check_every_ops: usize,
    pub adaptive_check_secs: usize,
    pub adaptive_activate_below_hitrate: f64,
    pub adaptive_confidence_factor: f64,
    pub adaptive_volume_factor: f64,
    pub adaptive_score_relax_factor: f64,
}

impl Default for RiskConfig {
    fn default() -> Self {
        Self::optimized_for_polygon()
    }
}

// ============================================================================
// 🪙 PARÂMETROS OTIMIZADOS PARA POLYGON — velocidade + limites reais
// ============================================================================
impl RiskConfig {
    pub fn optimized_for_polygon() -> Self {
        Self {
            min_profit_usd: 0.10,
            normal_min_profit_percent: 0.05,
            adaptive_min_profit_percent: 0.03,

            normal_gas_ratio: 5.0,
            adaptive_gas_ratio: 8.0,

            min_confidence: 0.40,
            min_volume_usd: 50.0,
            max_execution_risk: 0.90,
            max_risk_score: 40.0,

            default_token_decimals: 18,

            gas_dynamic_k: 0.00008,
            gas_min_usd: 0.0001,
            gas_max_usd: 0.05,
            gas_multiplier: 1.1,

            gas_dynamic_k_adaptive: 0.00005,
            gas_min_usd_adaptive: 0.0001,
            gas_max_usd_adaptive: 0.04,
            gas_multiplier_adaptive: 1.3,

            slippage_coef_aggressive: 0.45,
            slippage_min_abs_aggressive: 0.00001,
            slippage_max_abs_aggressive: 0.009,

            slippage_coef_adaptive: 0.60,
            slippage_min_abs_adaptive: 0.00001,
            slippage_max_abs_adaptive: 0.011,

            premium_usd_aggressive: 0.0,
            premium_usd_adaptive: 0.0,

            weight_negative_profit: 15.0,
            weight_profit_too_low: 2.0,
            weight_gas_too_high: 2.0,
            weight_low_confidence: 1.2,
            weight_low_volume: 0.4,
            weight_high_execution_risk: 3.5,

            weight_negative_profit_adaptive: 7.0,
            weight_profit_too_low_adaptive: 1.5,
            weight_gas_too_high_adaptive: 2.5,
            weight_low_confidence_adaptive: 1.0,
            weight_low_volume_adaptive: 0.5,
            weight_high_execution_risk_adaptive: 2.5,

            weight_step_penalty: 2.0,

            adaptive_check_every_ops: 8,
            adaptive_check_secs: 30,
            adaptive_activate_below_hitrate: 0.55,
            adaptive_confidence_factor: 0.15,
            adaptive_volume_factor: 0.07,
            adaptive_score_relax_factor: 0.90,
        }
    }
}

// ============================================================================
// 📦 Resultados de Execução
// ============================================================================
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BundleResult {
    pub success: bool,
    pub tx_hash: Option<String>,
    pub accepted: bool,
    pub profit: f64,
    pub gas_cost: f64,
    pub risk_assessment: Option<RiskAssessment>,
    #[serde(default)]
    pub execution_mode: Option<String>,
    /// Estado explícito da tentativa lógica de execução. `None` só em paths
    /// legados que ainda não foram migrados (paper/dry_run). Nunca colapsar
    /// `Reverted`/`TimeoutStuck`/`Dropped` em `None` ou em "skipped".
    #[serde(default)]
    pub outcome: Option<ExecutionOutcome>,
}

impl BundleResult {
    pub fn new(success: bool, profit: f64, gas_cost: f64) -> Self {
        Self {
            success,
            profit,
            gas_cost,
            tx_hash: None,
            accepted: success && profit > 0.0,
            risk_assessment: None,
            execution_mode: None,
            outcome: None,
        }
    }

    pub fn skipped() -> Self {
        Self {
            success: false,
            tx_hash: None,
            accepted: false,
            profit: 0.0,
            gas_cost: 0.0,
            risk_assessment: None,
            execution_mode: Some("skipped".to_string()),
            outcome: None,
        }
    }

    pub fn with_execution_mode(mut self, mode: &str) -> Self {
        self.execution_mode = Some(mode.to_string());
        self
    }

    pub fn with_tx_hash(mut self, tx_hash: Option<String>) -> Self {
        self.tx_hash = tx_hash;
        self
    }

    pub fn with_risk_assessment(mut self, assessment: Option<RiskAssessment>) -> Self {
        self.risk_assessment = assessment;
        self
    }

    /// Anexa o outcome explícito. `success`/`execution_mode` continuam como
    /// estavam para compat com telemetry legada, mas `outcome` é a fonte de
    /// verdade para classificação de execução.
    pub fn with_outcome(mut self, o: ExecutionOutcome) -> Self {
        self.outcome = Some(o);
        self
    }
}

// ============================================================================
// 📊 Diagnóstico / Métricas
// ============================================================================
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    pub avg_execution_time_ms: u64,
    pub success_rate: f64,
    pub hit_rate: f64,
    pub adaptive_mode: bool,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct Diagnostics {
    pub last_error: Option<String>,
    pub performance_metrics: PerformanceMetrics,
    pub risk_config: RiskConfig,
}

impl Diagnostics {
    pub fn record_error(&mut self, error: impl fmt::Display) {
        let msg = error.to_string();
        self.last_error = Some(msg);
        crate::infra::metrics::inc_errors("diagnostic_error");
    }

    pub fn update_metrics(&mut self, elapsed: u64, success: bool) {
        self.performance_metrics.avg_execution_time_ms = elapsed;
        self.performance_metrics.success_rate = if success { 100.0 } else { 0.0 };
        self.performance_metrics.hit_rate = crate::infra::metrics::get_hit_rate();
        crate::infra::metrics::observe_exec_latency_ms(elapsed as f64, "diagnostic");
    }

    pub fn update_risk_config(&mut self, config: RiskConfig) {
        self.risk_config = config.clone();
        crate::infra::metrics::set_adaptive_mode(
            config.adaptive_min_profit_percent < config.normal_min_profit_percent,
        );
    }

    pub fn should_activate_adaptive_mode(&self) -> bool {
        self.performance_metrics.hit_rate < 80.0
    }
}

// ============================================================================
// 🔄 Converters
// ============================================================================
impl From<Vec<ArbitrageStep>> for SerializableSteps {
    fn from(steps: Vec<ArbitrageStep>) -> Self {
        SerializableSteps(steps)
    }
}

impl From<SerializableSteps> for Vec<ArbitrageStep> {
    fn from(s: SerializableSteps) -> Self {
        s.0
    }
}

#[cfg(test)]
mod outcome_tests {
    use super::*;

    #[test]
    fn revert_is_not_skipped_and_distinct_from_same_block() {
        let h = H256::repeat_byte(0xAB);
        let reverted = ExecutionOutcome::Reverted {
            tx_hash: h,
            reason: Some("execution reverted".into()),
            gas_used: Some(U256::from(21_000)),
        };
        let same_block = ExecutionOutcome::SameBlockRejected { tx_hash: None };

        // Revert é execução on-chain falha — NUNCA skip.
        assert!(reverted.is_executed_onchain());
        assert!(reverted.is_reverted());
        assert!(!reverted.is_confirmed());
        // SameBlock é distinto de Reverted: não é execução on-chain.
        assert!(!same_block.is_executed_onchain());
        assert!(!same_block.is_reverted());
        assert_ne!(reverted, same_block);
    }

    #[test]
    fn confirmed_vs_loss_classified_by_sign() {
        let h = H256::repeat_byte(0x01);
        let profit = ExecutionOutcome::ConfirmedProfit {
            tx_hash: h,
            realized_profit_usd: 0.5,
            gas_used: U256::from(200_000),
        };
        let loss = ExecutionOutcome::ConfirmedLoss {
            tx_hash: h,
            realized_loss_usd: 0.1,
            gas_used: U256::from(200_000),
        };
        assert!(profit.is_confirmed());
        assert!(loss.is_confirmed());
        assert!(!profit.is_reverted());
        assert_ne!(profit, loss);
    }

    #[test]
    fn timeout_stuck_and_dropped_carry_nonce_distinct() {
        let n = U256::from(7);
        let stuck = ExecutionOutcome::TimeoutStuck {
            nonce: n,
            latest_tx_hash: None,
        };
        let dropped = ExecutionOutcome::Dropped { nonce: n };
        // Ambos carregam o nonce reservado (não sumiu); estados distintos.
        assert_ne!(stuck, dropped);
        assert!(!stuck.is_executed_onchain());
        assert!(!dropped.is_executed_onchain());
    }

    #[test]
    fn bundle_result_with_reverted_outcome_is_not_a_plain_skip() {
        let h = H256::repeat_byte(0x02);
        let res = BundleResult::skipped()
            .with_execution_mode("tx_reverted")
            .with_tx_hash(Some(format!("{:?}", h)))
            .with_outcome(ExecutionOutcome::Reverted {
                tx_hash: h,
                reason: None,
                gas_used: None,
            });
        // success/accepted falsos (não foi lucro), mas outcome explícito = Reverted.
        assert!(!res.success);
        assert_eq!(res.execution_mode.as_deref(), Some("tx_reverted"));
        assert!(res.outcome.as_ref().unwrap().is_reverted());
    }
}
