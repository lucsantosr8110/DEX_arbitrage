// ============================================================================
// src/core/flashloan.rs — FINAL v7.4 — CORREÇÕES CRÍTICAS APLICADAS
// ============================================================================

use ethers::abi::Tokenizable;
use crate::{
    config::Config,
    contracts::{FlashloanCaller, FlashloanExecutor, SwapStep as AbiSwapStep, ERC20},
    core::{
        arbitrage::ArbitrageEngine,
        economics,
        fixed_usd::{self, UsdE8},
        gas::{GasEstimator, GasStrategyKind},
        paper_validation::{self, PaperValidationHub},
        types::{ArbitrageOpportunity, BundleResult, ExecutionOutcome},
    },
    infra::metrics,
    utils::{u256_to_f64},
    AppMiddleware,
};

use anyhow::{anyhow, bail, Context, Result};
use ethers::{
    abi::{encode, Detokenize, Token},
    prelude::*,
    types::{Eip1559TransactionRequest, U256},
};
use k256::ecdsa::SigningKey;
use std::{str::FromStr, sync::Arc, time::Instant as StdInstant};
use tokio::{
    sync::Mutex,
    time::{timeout, Duration},
};
use tracing::{warn, info, debug};

const DEFAULT_AAVE_POOL_POLYGON: &str = "0x794a61358D6845594F94dc1DB02A252b5b4814aD";
const FLASHLOAN_PREMIUM_ABI: &str = r#"[{"inputs":[],"name":"FLASHLOAN_PREMIUM_TOTAL","outputs":[{"type":"uint128"}],"stateMutability":"view","type":"function"}]"#;

// ============================================================================
// Exec Strategy
// ============================================================================
#[derive(Debug, Clone, PartialEq)]
pub enum ExecutionStrategy {
    Direct,
    Flashloan,
    WrapperFlashloan,
    Skip,
}

impl std::fmt::Display for ExecutionStrategy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}", self)
    }
}

// ============================================================================
// Flashloan Error
// ============================================================================
#[derive(Debug, thiserror::Error)]
pub enum FlashloanError {
    #[error("Route too complex: {0}")]
    RouteTooComplex(String),
    
    #[error("Invalid route: {0}")]
    InvalidRoute(String),
    
    #[error("Slippage too high: {0}")]
    SlippageTooHigh(String),
    
    #[error("Insufficient profit: {0}")]
    InsufficientProfit(String),
    
    #[error("Execution failed: {0}")]
    ExecutionFailed(String),
    
    #[error("Configuration error: {0}")]
    ConfigError(String),
}

// ============================================================================
// Main Client
// ============================================================================
#[derive(Clone)]
pub struct ArbitrageClient {
    executor: FlashloanExecutor<AppMiddleware>,
    middleware: Arc<AppMiddleware>,
    gas_estimator: GasEstimator<AppMiddleware>,
    config: Arc<Mutex<Config>>,
    pub last_exec_block: Arc<Mutex<Option<u64>>>,
    pub execution_engine: Option<
        Arc<crate::execution::ExecutionEngine<AppMiddleware, Wallet<SigningKey>>>
    >,
    /// Hub paper (CSV async). Lazy-init sob flag.
    paper_hub: Arc<Mutex<Option<Arc<PaperValidationHub>>>>,
    /// Premium Aave on-chain cache (M5), refreshed at most hourly.
    flashloan_fee_cache: Arc<Mutex<Option<(StdInstant, f64)>>>,
}

impl ArbitrageClient {

    pub fn new(
        executor_address: Address,
        middleware: Arc<AppMiddleware>,
        config: Arc<Mutex<Config>>,
        execution_engine: Option<
            Arc<crate::execution::ExecutionEngine<AppMiddleware, Wallet<SigningKey>>>
        >,
    ) -> Self {

        let executor = FlashloanExecutor::new(executor_address, middleware.clone());
        let gas_estimator = GasEstimator::new(middleware.clone(), config.clone());

        // Env liga hub cedo; config.paper_enabled faz lazy-init no primeiro paper run.
        let paper_hub = if paper_validation::env_paper_flag() {
            info!("📄 PAPER_VALIDATION=1 | SENDS DISABLED | hub CSV ready");
            Arc::new(Mutex::new(Some(PaperValidationHub::spawn(
                std::path::PathBuf::from("audits/paper_validation.csv"),
                50,
            ))))
        } else {
            Arc::new(Mutex::new(None))
        };

        Self {
            executor,
            middleware,
            gas_estimator,
            config,
            execution_engine,
            last_exec_block: Arc::new(Mutex::new(None)),
            paper_hub,
            flashloan_fee_cache: Arc::new(Mutex::new(None)),
        }
    }

    async fn current_flashloan_fee_pct(
        &self,
        pool_address: Option<&str>,
        fallback_pct: f64,
    ) -> f64 {
        const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(3600);
        {
            let cache = self.flashloan_fee_cache.lock().await;
            if let Some((at, pct)) = *cache {
                if at.elapsed() < CACHE_TTL {
                    return pct;
                }
            }
        }

        let address = pool_address
            .and_then(|raw| Address::from_str(raw).ok())
            .or_else(|| Address::from_str(DEFAULT_AAVE_POOL_POLYGON).ok());
        let Some(address) = address else { return fallback_pct; };
        let Ok(abi) = serde_json::from_str::<ethers::abi::Abi>(FLASHLOAN_PREMIUM_ABI) else {
            return fallback_pct;
        };
        let pool = Contract::new(address, abi, self.middleware.clone());
        match pool.method::<_, U256>("FLASHLOAN_PREMIUM_TOTAL", ()) {
            Ok(call) => {
                match tokio::time::timeout(Duration::from_secs(10), call.call()).await {
                    Ok(Ok(bps)) if bps <= U256::from(10_000u64) => {
                        let pct = bps.as_u64() as f64 / 10_000.0;
                        *self.flashloan_fee_cache.lock().await = Some((StdInstant::now(), pct));
                        info!("Aave premium on-chain: {:.2} bps", pct * 10_000.0);
                        pct
                    }
                    Ok(Ok(_)) => {
                        warn!("⚠️ FLASHLOAN_PREMIUM_TOTAL fora do range; usando fallback");
                        fallback_pct
                    }
                    Ok(Err(e)) => {
                        warn!("⚠️ Falha ao ler FLASHLOAN_PREMIUM_TOTAL: {}; usando fallback", e);
                        fallback_pct
                    }
                    Err(_) => {
                        warn!("⏱️ Timeout ao ler FLASHLOAN_PREMIUM_TOTAL (>10s); usando fallback");
                        crate::infra::metrics::inc_counter("rpc_call_timeout");
                        fallback_pct
                    }
                }
            }
            Err(_) => fallback_pct,
        }
    }

    /// Lê premium no boot e sincroniza config compartilhada antes do radar.
    pub(crate) async fn refresh_flashloan_premium(&self) {
        let (enabled, pool, fallback) = {
            let cfg = self.config.lock().await;
            (
                cfg.flashloan.enabled,
                cfg.flashloan.aave_pool_address.clone(),
                cfg.flashloan.fee_pct.unwrap_or(economics::AAVE_V3_PREMIUM_PCT),
            )
        };
        if !enabled {
            return;
        }
        let pct = self.current_flashloan_fee_pct(pool.as_deref(), fallback).await;
        self.config.lock().await.flashloan.fee_pct = Some(pct);
    }

    async fn ensure_paper_hub(&self) -> Option<Arc<PaperValidationHub>> {
        let (enabled, path, window) = {
            let cfg = self.config.lock().await;
            (
                // Mesmo critério da observação: paper / dry_run_only / dry_run
                paper_validation::observation_active(&cfg),
                cfg.validation.csv_path.clone(),
                cfg.validation.summary_window.max(1),
            )
        };
        if !enabled {
            return None;
        }
        let mut slot = self.paper_hub.lock().await;
        if slot.is_none() {
            info!(
                "📄 PAPER hub start | csv={} | summary_window={} | SENDS DISABLED",
                path, window
            );
            crate::dex::reset_fee100_best_discarded_count();
            crate::dex::liquidity::reset_low_liquidity_discarded_count();
            crate::core::arbitrage::reset_triangular_leg_low_liquidity_discarded_count();
            *slot = Some(PaperValidationHub::spawn(
                std::path::PathBuf::from(path),
                window,
            ));
        }
        slot.clone()
    }

    // ========================================================================
    // 🚨 VALIDAÇÕES CRÍTICAS - CORRIGIDAS
    // ========================================================================

    /// Fee do flashloan em unidades de token.
    /// `fee_pct` vem de `config.flashloan.fee_pct` (ex.: 0.0005 = 5 bps Aave V3).
    /// TODO: opcionalmente ler `FLASHLOAN_PREMIUM_TOTAL` on-chain do Aave Pool.
    fn calculate_flashloan_fee(&self, amount: U256, fee_pct: f64) -> U256 {
        if amount.is_zero() || !fee_pct.is_finite() || fee_pct <= 0.0 {
            return U256::zero();
        }
        let fee_bps = (fee_pct * 10_000.0).round() as u64;
        if fee_bps == 0 {
            return U256::zero();
        }
        amount * U256::from(fee_bps) / U256::from(10_000u64)
    }

    /// Aplica slippage em bps (helper; o path de execução NÃO usa — slippage
    /// única fica em `ArbitrageEngine::apply_slippage_safe`).
    #[cfg(test)]
    fn apply_slippage(&self, amount: U256, slippage_bps: u64) -> U256 {
        let slippage_factor = U256::from(10000 - slippage_bps);
        amount * slippage_factor / U256::from(10000)
    }

    /// Convenção A5 (**gross-based**, uma fonte de verdade):
    /// recebe GROSS profit e recalcula
    ///   `net = gross - gas_cost_usd - flashloan_fee_usd`
    /// onde `flashloan_fee_usd` já foi derivado de `config.flashloan.fee_pct`.
    /// NÃO recebe net pré-descontado — evita dupla dedução de gas/Aave.
    /// Slippage continua filtrada no engine (`recalculate_profitability`).
    fn validate_profit_after_fees(
        &self,
        gross_profit_usd: f64,
        gas_cost_usd: f64,
        flashloan_fee_usd: f64,
        adverse_move_usd: f64,
    ) -> Result<(), FlashloanError> {
        let net_profit_usd = economics::net_profit_usd(
            gross_profit_usd,
            &economics::TradeCosts {
                gas_usd: gas_cost_usd,
                flashloan_fee_usd,
                adverse_move_usd,
            },
        );

        if net_profit_usd <= 0.0 {
            return Err(FlashloanError::InsufficientProfit(format!(
                "Net profit ${:.4} <= 0 (gross: ${:.4}, gas: ${:.4}, flashloan_fee: ${:.4}, adverse: ${:.4})",
                net_profit_usd, gross_profit_usd, gas_cost_usd, flashloan_fee_usd, adverse_move_usd
            )));
        }

        info!(
            "💰 Profit validation: ${:.4} net (${:.4} gross - ${:.4} gas - ${:.4} flashloan - ${:.4} adverse)",
            net_profit_usd, gross_profit_usd, gas_cost_usd, flashloan_fee_usd, adverse_move_usd
        );

        Ok(())
    }

    /// Integer gate: net = gross - gas - flashloan_fee - adverse (`UsdE8`).
    fn validate_profit_after_fees_e8(
        &self,
        gross: UsdE8,
        gas: UsdE8,
        flashloan_fee: UsdE8,
        adverse: UsdE8,
    ) -> Result<(), FlashloanError> {
        let net = fixed_usd::net_profit_usd_e8(gross, gas, flashloan_fee, adverse);
        if net.0 == 0 {
            return Err(FlashloanError::InsufficientProfit(format!(
                "Net profit ${:.4} <= 0 (gross: ${:.4}, gas: ${:.4}, flashloan_fee: ${:.4}, adverse: ${:.4})",
                net.display_f64(),
                gross.display_f64(),
                gas.display_f64(),
                flashloan_fee.display_f64(),
                adverse.display_f64()
            )));
        }
        info!(
            "💰 Profit validation: ${:.4} net (${:.4} gross - ${:.4} gas - ${:.4} flashloan - ${:.4} adverse)",
            net.display_f64(),
            gross.display_f64(),
            gas.display_f64(),
            flashloan_fee.display_f64(),
            adverse.display_f64()
        );
        Ok(())
    }

    /// Telemetry/UI only — converts raw→USD via f64 display helper.
    /// Economic gates must use `fixed_usd::token_raw_to_usd_e8`.
    fn token_amount_to_usd_display_f64(&self, amount: U256, price_usd: f64, decimals: u32) -> f64 {
        if !price_usd.is_finite() || price_usd <= 0.0 {
            return 0.0;
        }
        fixed_usd::token_raw_display_f64(amount, decimals) * price_usd
    }

    fn gas_strategy_kind(strategy: &ExecutionStrategy) -> GasStrategyKind {
        match strategy {
            ExecutionStrategy::Direct => GasStrategyKind::Direct,
            ExecutionStrategy::Flashloan
            | ExecutionStrategy::WrapperFlashloan
            | ExecutionStrategy::Skip => GasStrategyKind::WithFlashloan,
        }
    }

    /// Economic profit recipient: must match on-chain `owner` (Q1).
    /// Prefer `[wrapper].owner_address`, else signing wallet.
    fn resolve_profit_recipient(&self, cfg: &Config) -> Result<Address> {
        if let Some(ref owner) = cfg.wrapper.owner_address {
            let trimmed = owner.trim();
            if !trimmed.is_empty() {
                return Address::from_str(trimmed)
                    .with_context(|| format!("invalid wrapper.owner_address={owner}"));
            }
        }
        self.get_wallet_address()
    }

    /// ABI-encode `uint24` fee tier — layout compatível com
    /// `abi.decode(extraData, (uint24))` em `FlashloanExecutor.sol`.
    pub(crate) fn encode_v3_fee_extra_data(fee_tier: u32) -> Bytes {
        Bytes::from(encode(&[Token::Uint(U256::from(fee_tier))]))
    }

    /// Decode round-trip de teste / verificação (uint24 ABI-padded).
    #[cfg(test)]
    pub(crate) fn decode_v3_fee_extra_data(data: &Bytes) -> Result<u32> {
        let tokens = ethers::abi::decode(&[ethers::abi::ParamType::Uint(256)], data.as_ref())
            .map_err(|e| anyhow!("decode v3 fee: {e}"))?;
        let Token::Uint(v) = tokens
            .into_iter()
            .next()
            .ok_or_else(|| anyhow!("empty decode"))?
        else {
            return Err(anyhow!("expected Uint"));
        };
        Ok(v.as_u32())
    }

    /// Fee tiers que o executor on-chain aceita — delega à fonte única em `dex::`.
    fn executable_v3_fee_tier(fee_tier: u32) -> Result<u32> {
        if crate::dex::is_executable_v3_fee_tier(fee_tier) {
            Ok(fee_tier)
        } else if fee_tier == crate::dex::OBSERVED_V3_FEE_TIER_100 {
            Err(anyhow!(
                "V3 fee_tier=100 cotado mas executor rejeita (só 500|3000|10000) — abort opp"
            ))
        } else {
            Err(anyhow!(
                "V3 fee_tier={} inválido para executor — abort opp",
                fee_tier
            ))
        }
    }

    fn is_uniswap_v3_step(dex_name: &str) -> bool {
        let n = dex_name
            .to_lowercase()
            .replace(' ', "")
            .replace('_', "");
        n.contains("uniswapv3") || n == "uniswapv3"
    }

    /// Resolve fee V3 **congelado na detecção** (`step.v3_fee_tier`).
    /// Não re-consulta cache para não re-otimizar fee entre detecção e eth_call.
    fn resolve_v3_fee_for_step(step: &crate::core::types::ArbitrageStep) -> Result<u32> {
        let fee = step.v3_fee_tier.ok_or_else(|| {
            anyhow!(
                "V3 fee_tier ausente no step {}→{} — abort (não forçar 3000 / não re-query)",
                step.token_in, step.token_out
            )
        })?;
        Self::executable_v3_fee_tier(fee)
    }

    /// Monta `extraData`: V3 = abi.encode(uint24); V2/Curve = vazio.
    pub(crate) fn build_extra_data_for_step(step: &crate::core::types::ArbitrageStep) -> Result<Bytes> {
        if Self::is_uniswap_v3_step(&step.dex_name) {
            let fee = Self::resolve_v3_fee_for_step(step)?;
            Ok(Self::encode_v3_fee_extra_data(fee))
        } else {
            Ok(Bytes::new())
        }
    }

    /// Valida steps críticos
    fn validate_steps_critical(&self, steps: &[AbiSwapStep]) -> Result<(), FlashloanError> {
        if steps.is_empty() {
            return Err(FlashloanError::InvalidRoute("Empty steps".into()));
        }
        
        for (i, step) in steps.iter().enumerate() {
            if step.amount_out_min.is_zero() {
                return Err(FlashloanError::InvalidRoute(
                    format!("Step {}: amount_out_min is zero", i)
                ));
            }
        }
        
        self.validate_route_consistency(steps)?;
        
        Ok(())
    }

    /// CORREÇÃO: Valida complexidade sem bloquear arbitragem triangular
    fn validate_route_complexity(&self, steps: &[AbiSwapStep], config: &Config) -> Result<(), FlashloanError> {
        let route = &config.arbitrage.route_validation;
        let max_hops_allowed = if route.enabled {
            config.arbitrage.max_path_length.min(route.max_hops.max(1)) as usize
        } else {
            config.arbitrage.max_path_length as usize
        };
        
        // CORREÇÃO: Permitir até 4 hops para arbitragem triangular
        if steps.len() > max_hops_allowed {
            let reason = format!("{} hops exceeds maximum {}", steps.len(), max_hops_allowed);
            self.log_route_rejection(steps, &reason);
            return Err(FlashloanError::RouteTooComplex(reason));
        }
        
        if route.enabled && route.block_same_dex_consecutive {
            for pair in steps.windows(2) {
                if pair[0].dex_type == pair[1].dex_type {
                    let reason = "consecutive hops on same DEX are blocked".to_string();
                    self.log_route_rejection(steps, &reason);
                    return Err(FlashloanError::InvalidRoute(reason));
                }
            }
        }

        if route.enabled && route.reject_high_slippage_routes {
            // `amount_out_min` já carrega orçamento real calculado pelo engine.
            // Não rejeitar pelo pior caso teórico: isso bloqueava toda rota de
            // 3 hops mesmo com budget efetivo de poucos bps por perna.
            let hops = steps.len() as u64;
            let configured_limit_bps = (route.max_cumulative_slippage * 100.0).floor();
            if !configured_limit_bps.is_finite()
                || configured_limit_bps < 0.0
                || (configured_limit_bps as u64)
                    < hops.saturating_mul(crate::core::economics::MIN_SLIPPAGE_BPS as u64)
            {
                let reason = format!(
                    "route slippage limit {:.0} bps cannot preserve {} bps floor for {} hops",
                    configured_limit_bps,
                    crate::core::economics::MIN_SLIPPAGE_BPS,
                    hops
                );
                self.log_route_rejection(steps, &reason);
                return Err(FlashloanError::SlippageTooHigh(reason));
            }
        }

        Ok(())
    }

    /// Valida consistência básica da rota
    fn validate_route_consistency(&self, steps: &[AbiSwapStep]) -> Result<(), FlashloanError> {
        for i in 0..steps.len() - 1 {
            if steps[i].token_out != steps[i + 1].token_in {
                return Err(FlashloanError::InvalidRoute(
                    format!("Step {}: token_out {:?} != next token_in {:?}", 
                           i, steps[i].token_out, steps[i + 1].token_in)
                ));
            }
        }
        
        // CORREÇÃO: Mantida validação de ciclo - importante para flashloan
        if steps[0].token_in != steps[steps.len() - 1].token_out {
            return Err(FlashloanError::InvalidRoute(
                format!("Route doesn't return to initial token: {:?} != {:?}", 
                       steps[0].token_in, steps[steps.len() - 1].token_out)
            ));
        }
        
        Ok(())
    }

    /// Aplica filtros de complexidade
    fn apply_complexity_filters(&self, steps: &[AbiSwapStep], config: &Config) -> Result<(), FlashloanError> {
        self.validate_route_complexity(steps, config)?;
        self.validate_steps_critical(steps)?;
        
        if config.arbitrage.advanced_filters_enabled && steps.len() >= 3 {
            info!("🔍 Rota complexa detectada ({} hops) - monitorando", steps.len());
        }
        
        Ok(())
    }

    /// Log detalhado para rejeição de rotas
    fn log_route_rejection(&self, steps: &[AbiSwapStep], reason: &str) {
        let route_desc = self.format_route_for_log(steps);
        warn!("🚨 Rota rejeitada: {} | Razão: {}", route_desc, reason);
    }

    /// Formata rota para logging claro
    fn format_route_for_log(&self, steps: &[AbiSwapStep]) -> String {
        if steps.is_empty() {
            return "Empty".to_string();
        }
        
        let mut route = format!("{:?}", steps[0].token_in);
        for step in steps {
            route.push_str(&format!(" → {:?}", step.token_out));
        }
        
        route
    }

    // ========================================================================
    // EXECUTAR OPORTUNIDADE - COM CORREÇÕES ANTI-MEV
    // ========================================================================
    pub async fn execute_opportunity(
        &self,
        opp: &mut ArbitrageOpportunity
    ) -> Result<BundleResult> {

        // NOTE: update_execution_block() era chamado AQUI (antes da execução),
        // o que faria debounce_same_block() sempre retornar true se fosse
        // implementado. Movido para após TX confirmada em send_and_confirm_transaction.
        let (strategy, min_profit, risk_cfg, slippage_bps, flashloan_decimals, configured_fee_pct, pool_address, max_premium_bps, adverse_move_bps) = {
            let cfg = self.config.lock().await;

            (
    self.determine_execution_strategy(opp, &cfg).await,
    cfg.arbitrage.min_profit_absolute.parse::<f64>().unwrap_or(0.0001),
    cfg.risk.clone(),
    cfg.flashloan.slippage_bps.unwrap_or(50) as u64,
    // 6 decimais (USDC/USDT) — MESMO default dos outros dois call sites.
    // Divergia (18 aqui, 6 nos demais): com o campo ausente no TOML a fee do
    // flashloan saía dividida por 1e18 em vez de 1e6, virando ~zero.
    cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32,
    // Mesma fonte do engine (`recalculate_profitability`); default 5 bps Aave V3.
    cfg.flashloan.fee_pct.unwrap_or(economics::AAVE_V3_PREMIUM_PCT),
    cfg.flashloan.aave_pool_address.clone(),
    cfg.flashloan.max_premium_bps,
    cfg.execution.adverse_move_bps,
)
        };

        // Direct = capital próprio: sem premium Aave.
        let fee_pct = if matches!(strategy, ExecutionStrategy::Direct) {
            0.0
        } else {
            self.current_flashloan_fee_pct(pool_address.as_deref(), configured_fee_pct)
                .await
        };

        // A11: `flashloan.slippage_bps` é parâmetro morto — não controla slippagem
        // algum. A slippagem real vem de `execution.max_slippage_bps` + `safety_margin_bps`
        // via `apply_slippage_safe` (core/arbitrage.rs). Avisar o operador que ajustar
        // este campo no TOML não muda comportamento, p/ evitar falso sentimento de
        // controle de risco.
        if self.config.lock().await.flashloan.slippage_bps.is_some() {
            warn!(
                "flashloan.slippage_bps está definido no TOML mas é ignorado: a slippagem \
                 real é controlada por execution.max_slippage_bps + safety_margin_bps. \
                 Remova o campo ou migre para execution.max_slippage_bps."
            );
        }

        let fee_bps = match fixed_usd::fee_pct_to_bps(fee_pct) {
            Ok(b) => b,
            Err(e) => {
                warn!("Flashloan skip: invalid fee_pct: {e}");
                return Ok(BundleResult::skipped().with_execution_mode("premium_cap"));
            }
        };
        if max_premium_bps
            .map(|limit| fee_bps > u64::from(limit))
            .unwrap_or(false)
        {
            warn!(
                "Flashloan skip: fee_bps={} exceeds max_premium_bps={:?}",
                fee_bps, max_premium_bps
            );
            return Ok(BundleResult::skipped().with_execution_mode("premium_cap"));
        }

        // GAS — overhead Aave tipado por estratégia.
        let n_hops = opp.steps.0.len().max(1);
        let gas_kind = Self::gas_strategy_kind(&strategy);
        let gas_cost = match self
            .gas_estimator
            .estimate_gas_usd_for_hops(n_hops, gas_kind)
            .await
        {
            Ok(v) => {
                opp.gas_cost_usd = v;
                metrics::set_last_gas_usd(v);
                v
            }
            Err(_) => opp.gas_cost_usd,
        };

        // Integer economic gate (no f64 approve/reject).
        let price_e8 = match fixed_usd::price_usd_e8_from_option(opp.token_price_usd) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "MissingProfitTokenPrice — abort exec");
                metrics::inc_counter("missing_profit_token_price");
                return Ok(BundleResult::skipped().with_execution_mode("missing_token_price"));
            }
        };
        let principal_e8 =
            match fixed_usd::token_raw_to_usd_e8(opp.amount_in, price_e8, flashloan_decimals) {
                Ok(v) => v,
                Err(e) => {
                    warn!("principal usd_e8 failed: {e}");
                    return Ok(BundleResult::skipped().with_execution_mode("principal_usd_overflow"));
                }
            };
        let flashloan_fee_e8 = match fixed_usd::flashloan_fee_usd_e8(principal_e8, fee_bps) {
            Ok(v) => v,
            Err(e) => {
                warn!("flashloan fee usd_e8 failed: {e}");
                return Ok(BundleResult::skipped().with_execution_mode("fee_usd_overflow"));
            }
        };
        let gas_e8 = match fixed_usd::usd_f64_to_e8_ceil(gas_cost) {
            Ok(v) => v,
            Err(e) => {
                warn!("gas usd_e8 failed: {e}");
                return Ok(BundleResult::skipped().with_execution_mode("gas_usd_invalid"));
            }
        };
        let gross_e8 = match fixed_usd::usd_f64_to_e8_ceil(opp.estimated_profit_usd) {
            Ok(v) => v,
            Err(e) => {
                warn!("gross profit usd_e8 failed: {e}");
                return Ok(BundleResult::skipped().with_execution_mode("gross_usd_invalid"));
            }
        };
        let adverse_e8 = {
            let adverse_f =
                economics::adverse_move_usd(opp.estimated_volume_usd.max(0.0), adverse_move_bps);
            match fixed_usd::usd_f64_to_e8_ceil(adverse_f) {
                Ok(v) => v,
                Err(e) => {
                    warn!("adverse usd_e8 failed: {e}");
                    return Ok(BundleResult::skipped().with_execution_mode("adverse_usd_invalid"));
                }
            }
        };

        if let Err(e) =
            self.validate_profit_after_fees_e8(gross_e8, gas_e8, flashloan_fee_e8, adverse_e8)
        {
            warn!("{}", e);
            return Ok(BundleResult::skipped().with_execution_mode("insufficient_profit"));
        }

        let flashloan_fee_usd = flashloan_fee_e8.display_f64();
        let net_e8 =
            fixed_usd::net_profit_usd_e8(gross_e8, gas_e8, flashloan_fee_e8, adverse_e8);
        opp.net_profit_usd = net_e8.display_f64();

        let min_profit_e8 = match fixed_usd::usd_f64_to_e8_ceil(min_profit) {
            Ok(v) => v,
            Err(e) => {
                warn!("min_profit usd_e8 failed: {e}");
                return Ok(BundleResult::skipped().with_execution_mode("min_profit_invalid"));
            }
        };
        if net_e8.0 < min_profit_e8.0 {
            info!(
                "💰 Profit skip: ${:.4} < ${:.4}",
                opp.net_profit_usd, min_profit
            );
            return Ok(BundleResult::skipped().with_execution_mode("profit_skip"));
        }

        // RISK MANAGER
        if let Some(risk_mgr) = crate::core::risk::RISK_MANAGER.get() {
            let mut guard = risk_mgr.lock().await;
            guard.config = risk_cfg;

            if !guard.assess_opportunity(opp).approved {
                return Ok(BundleResult::skipped().with_execution_mode("risk_reject"));
            }
        }

        // PAPER GATE: eth_call + delta; nunca chega em send.
        {
            let paper_on = {
                let cfg = self.config.lock().await;
                paper_validation::paper_mode_active(&cfg)
            };
            if paper_on {
                let would = {
                    let cfg = self.config.lock().await;
                    paper_validation::would_execute(opp.spread_percent, &cfg)
                };
                return self
                    .run_paper_validation(opp, slippage_bps, flashloan_fee_usd, would)
                    .await;
            }
        }

        info!("🔄 Executando rota: {} hops | Profit: ${:.4}", 
              opp.steps.0.len(), opp.net_profit_usd);

        // EXEC
        match strategy {
            ExecutionStrategy::Direct => self.execute_direct(opp, slippage_bps).await,
            ExecutionStrategy::Flashloan => self.execute_flashloan(opp, slippage_bps).await,
            ExecutionStrategy::WrapperFlashloan => self.execute_wrapper(opp, slippage_bps).await,
            ExecutionStrategy::Skip => Ok(BundleResult::skipped()),
        }
    }

    /// Paper observe público (bot observation path) — bypass min_profit de exec.
    /// Envio continua bloqueado por `sends_forbidden`.
    /// Falhas de encode/sim ainda geram amostra CSV (sim_ok=false).
    pub async fn paper_observe_opportunity(
        &self,
        opp: &mut ArbitrageOpportunity,
        would_execute: bool,
    ) -> Result<BundleResult> {
        {
            let cfg = self.config.lock().await;
            if !paper_validation::observation_active(&cfg) {
                return Err(anyhow!(
                    "paper_observe_opportunity só em observation/paper mode"
                ));
            }
            if !paper_validation::sends_forbidden(&cfg) {
                return Err(anyhow!(
                    "invariant broken: observation_active sem sends_forbidden"
                ));
            }
        }

        let (slippage_bps, configured_fee_pct, pool_address, flashloan_decimals, adverse_move_bps) = {
            let cfg = self.config.lock().await;
            (
                cfg.flashloan.slippage_bps.unwrap_or(50) as u64,
                cfg.flashloan.fee_pct.unwrap_or(economics::AAVE_V3_PREMIUM_PCT),
                cfg.flashloan.aave_pool_address.clone(),
                cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32,
                cfg.execution.adverse_move_bps,
            )
        };

        let fee_pct = self
            .current_flashloan_fee_pct(pool_address.as_deref(), configured_fee_pct)
            .await;

        // A6: paper valida flashloan → overhead WithFlashloan.
        let n_hops = opp.steps.0.len().max(1);
        let gas_cost = match self
            .gas_estimator
            .estimate_gas_usd_for_hops(n_hops, GasStrategyKind::WithFlashloan)
            .await
        {
            Ok(v) => {
                opp.gas_cost_usd = v;
                v
            }
            Err(_) => opp.gas_cost_usd,
        };

        let price_e8 = match fixed_usd::price_usd_e8_from_option(opp.token_price_usd) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "MissingProfitTokenPrice — abort paper");
                metrics::inc_counter("missing_profit_token_price");
                return Ok(BundleResult::skipped().with_execution_mode("missing_token_price"));
            }
        };
        let fee_bps = fixed_usd::fee_pct_to_bps(fee_pct).unwrap_or(0);
        let principal_e8 =
            fixed_usd::token_raw_to_usd_e8(opp.amount_in, price_e8, flashloan_decimals)
                .unwrap_or(UsdE8::zero());
        let flashloan_fee_e8 =
            fixed_usd::flashloan_fee_usd_e8(principal_e8, fee_bps).unwrap_or(UsdE8::zero());
        let gas_e8 = fixed_usd::usd_f64_to_e8_ceil(gas_cost).unwrap_or(UsdE8::zero());
        let gross_e8 =
            fixed_usd::usd_f64_to_e8_ceil(opp.estimated_profit_usd).unwrap_or(UsdE8::zero());
        let adverse_e8 = fixed_usd::usd_f64_to_e8_ceil(economics::adverse_move_usd(
            opp.estimated_volume_usd.max(0.0),
            adverse_move_bps,
        ))
        .unwrap_or(UsdE8::zero());
        let flashloan_fee_usd = flashloan_fee_e8.display_f64();
        opp.net_profit_usd =
            fixed_usd::net_profit_usd_e8(gross_e8, gas_e8, flashloan_fee_e8, adverse_e8)
                .display_f64();

        match self
            .run_paper_validation(opp, slippage_bps, flashloan_fee_usd, would_execute)
            .await
        {
            Ok(r) => Ok(r),
            Err(e) => {
                // Ainda grava amostra para calibrar (motivo no revert_reason).
                let hub = self.ensure_paper_hub().await;
                let block = paper_validation::current_block_number(&self.middleware)
                    .await
                    .unwrap_or(0);
                let sample = paper_validation::build_sample(
                    opp,
                    block,
                    flashloan_fee_usd,
                    Some(0.0),
                    false,
                    Some(format!("{:#}", e)),
                    would_execute,
                );
                paper_validation::log_sample(&sample);
                if let Some(h) = hub {
                    h.try_submit(sample);
                }
                Ok(BundleResult::skipped().with_execution_mode("paper_observe_error"))
            }
        }
    }

    /// Paper: mesmo calldata/extraData da exec real; só eth_call; CSV async.
    ///
    /// `from` = endereço público autorizado (`paper_from` / owner) — **nunca** keypair.
    /// Se `executeFlashloan` retorna `false` (try/catch), faz probe Aave para
    /// revelar o revert interno ("Not profitable", etc.).
    async fn run_paper_validation(
        &self,
        opp: &ArbitrageOpportunity,
        slippage_bps: u64,
        flashloan_fee_usd: f64,
        would_execute: bool,
    ) -> Result<BundleResult> {
        let hub = self.ensure_paper_hub().await;
        let block = paper_validation::current_block_number(&self.middleware)
            .await
            .unwrap_or(0);
        let sample = self
            .paper_validate_at_block(opp, block, slippage_bps, flashloan_fee_usd, would_execute)
            .await?;
        paper_validation::log_sample(&sample);
        if let Some(h) = hub {
            h.try_submit(sample);
        }
        Ok(BundleResult::skipped().with_execution_mode("paper_validated"))
    }

    /// Paper eth_call no `blockTag` dado (replay histórico). Mesmo from público;
    /// **nunca** send. Retorna amostra sem side-effects de hub (caller decide).
    pub async fn paper_validate_at_block(
        &self,
        opp: &ArbitrageOpportunity,
        block: u64,
        slippage_bps: u64,
        flashloan_fee_usd: f64,
        would_execute: bool,
    ) -> Result<paper_validation::PaperSample> {
        let (asset, amount, steps, decimals, token_price, paper_from, use_overrides) = {
            let cfg = self.config.lock().await;
            let (asset, amount, steps) =
                self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;
            let wallet = self.get_wallet_address()?;
            let paper_from = paper_validation::resolve_paper_from(&cfg, wallet);
            (
                asset,
                amount,
                steps,
                cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32,
                opp.token_price_usd.unwrap_or(1.0),
                paper_from,
                paper_validation::paper_state_overrides_enabled(&cfg),
            )
        };

        info!(
            target: "paper_validation",
            paper_from = ?paper_from,
            block,
            "PAPER eth_call@block from=paper_from (sem assinatura)"
        );

        let block_id = paper_validation::block_id(block);
        let holder = paper_from;

        let _bal_before = paper_validation::erc20_balance(
            self.middleware.clone(),
            asset,
            holder,
            block_id,
        )
        .await
        .ok();

        let call = self
            .executor
            .execute_flashloan(asset, amount, steps.clone(), U256::zero())
            .from(paper_from)
            .block(block_id);

        let (sim_ok, revert_reason) = match timeout(Duration::from_secs(15), call.call()).await {
            Ok(Ok(true)) => (true, None),
            Ok(Ok(false)) => {
                let params = self.encode_flashloan_callback_params(paper_from, &steps, U256::zero());
                let state_ovr = if use_overrides {
                    Some(paper_validation::erc20_balance_state_override(
                        asset,
                        holder,
                        amount.saturating_mul(U256::from(2u64)),
                    ))
                } else {
                    None
                };
                let probed = paper_validation::probe_aave_flashloan_revert(
                    &self.middleware,
                    paper_from,
                    self.executor.address(),
                    asset,
                    amount,
                    params,
                    block_id,
                    state_ovr,
                )
                .await;
                (
                    false,
                    Some(format!(
                        "{}; {}",
                        paper_validation::GENERIC_FLASHLOAN_FALSE,
                        probed
                    )),
                )
            }
            Ok(Err(e)) => {
                let decoded = paper_validation::decode_revert_message(&e.to_string());
                if paper_validation::is_archive_state_error(&decoded)
                    || paper_validation::is_archive_state_error(&e.to_string())
                {
                    return Err(anyhow!(
                        "archive state unavailable at block {block}: {decoded}"
                    ));
                }
                (false, Some(format!("executeFlashloan revert: {decoded}")))
            }
            Err(_) => (false, Some("Simulation timeout".into())),
        };

        let mut profit_realizado_usd = None;
        if sim_ok {
            if let Some(data) = call.tx.data().cloned() {
                let to = match call.tx.to() {
                    Some(NameOrAddress::Address(a)) => *a,
                    _ => self.executor.address(),
                };
                if let Some(delta_raw) = paper_validation::try_alchemy_asset_delta(
                    &self.middleware,
                    paper_from,
                    to,
                    data,
                    asset,
                    holder,
                )
                .await
                {
                    profit_realizado_usd = Some(paper_validation::balance_delta_usd(
                        delta_raw,
                        decimals,
                        token_price,
                    ));
                }
            }
        }

        Ok(paper_validation::build_sample(
            opp,
            block,
            flashloan_fee_usd,
            profit_realizado_usd,
            sim_ok,
            revert_reason,
            would_execute,
        ))
    }

    /// `abi.encode(profitRecipient, steps, minProfit)` — layout `CallbackData`
    /// (campo renomeado de `executor`; ordem/tipos inalterados).
    pub(crate) fn encode_callback_data(
        profit_recipient: Address,
        steps: &[AbiSwapStep],
        min_profit: U256,
    ) -> Bytes {
        use ethers::abi::Tokenizable;
        Bytes::from(ethers::abi::encode(&[
            profit_recipient.into_token(),
            steps.to_vec().into_token(),
            min_profit.into_token(),
        ]))
    }

    /// `abi.encode(initiator, steps, minProfit)` — callback do executor.
    fn encode_flashloan_callback_params(
        &self,
        initiator: Address,
        steps: &[AbiSwapStep],
        min_profit: U256,
    ) -> Bytes {
        Self::encode_callback_data(initiator, steps, min_profit)
    }

    /// Fail-closed: `profitRecipient` resolvido deve == `owner()` on-chain.
    async fn assert_profit_recipient_matches_owner(
        &self,
        profit_recipient: Address,
    ) -> Result<()> {
        let onchain_owner: Address = self.executor.owner().call().await.map_err(|e| {
            anyhow!("ProfitRecipientPreflight: failed to read owner(): {e}")
        })?;
        Self::ensure_profit_recipient_matches_owner(profit_recipient, onchain_owner)
    }

    /// Pure gate used by preflight + unit tests (no RPC).
    fn ensure_profit_recipient_matches_owner(
        profit_recipient: Address,
        onchain_owner: Address,
    ) -> Result<()> {
        if profit_recipient != onchain_owner {
            metrics::inc_counter("profit_recipient_owner_mismatch");
            bail!(
                "ProfitRecipientMismatch: resolved={profit_recipient:?} \
                 onchain_owner={onchain_owner:?} — abort before broadcast"
            );
        }
        Ok(())
    }

    /// Piso on-chain em raw units do ativo de entrada.
    ///
    /// `ceil((gas_usd_e8 + min_profit_usd_e8) * 10^decimals / price_e8)`.
    /// Fail-closed se `token_price_usd` ausente/≤0 (`MissingProfitTokenPrice`).
    fn minimum_profit_raw(
        &self,
        opp: &ArbitrageOpportunity,
        decimals: u32,
        cfg: &Config,
    ) -> Result<U256> {
        let price_e8 = fixed_usd::price_usd_e8_from_option(opp.token_price_usd).map_err(|e| {
            metrics::inc_counter("missing_profit_token_price");
            e
        })?;
        let gas_e8 = fixed_usd::usd_f64_to_e8_ceil(opp.gas_cost_usd)?;
        let min_e8 = fixed_usd::usd_str_to_e8_ceil(&cfg.arbitrage.min_profit_absolute)?;
        let required = gas_e8.checked_add(min_e8)?;
        fixed_usd::usd_e8_to_token_raw_ceil(required, price_e8, decimals)
    }

    // ========================================================================
    // CORREÇÃO: ANTI-MEV NÃO BLOQUEADOR
    // ========================================================================
    async fn update_execution_block(&self) {
        if let Ok(block) = self.middleware.get_block_number().await {
            let block = block.as_u64();
            let mut guard = self.last_exec_block.lock().await;
            *guard = Some(block);
            debug!("📦 Bloco atualizado: {}", block);
        }
    }

    /// Anti-MEV: checa se já executamos neste bloco.
    ///
    /// O contrato Solidity tem `modifier antiMEV { require(block.number !=
    /// lastExecutionBlock) }` em executeFlashloan e executeDirect. Sem esta
    /// verificação no lado Rust, a segunda execução no mesmo bloco reverte
    /// on-chain, queimando gás. Retorna true se o bloco atual == último bloco
    /// de execução (deve skipar).
    async fn debounce_same_block(&self) -> Result<bool> {
        let last = {
            let guard = self.last_exec_block.lock().await;
            *guard
        };
        let Some(last_block) = last else {
            // Primeira execução ou após restart — sem histórico, permite.
            return Ok(false);
        };
        let current = self.middleware.get_block_number().await?.as_u64();
        if current == last_block {
            debug!("🛡️ Anti-MEV: bloco {} já teve execução, skipando", current);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    // ========================================================================
    // Estratégia
    // ========================================================================
    async fn determine_execution_strategy(
        &self,
        _opp: &ArbitrageOpportunity,
        cfg: &Config
    ) -> ExecutionStrategy {
        // `dry_run` NÃO é mais um curto-circuito aqui. Antes ele retornava `Skip`
        // antes de qualquer coisa, então `simulate_before_execute` nunca rodava — o
        // dry run só validava aritmética de spread, nunca se a rota executaria.
        //
        // Agora o dry run escolhe a mesma estratégia da execução real e segue o
        // caminho normal; cada `execute_*` tem uma guarda `if dry { return }` DEPOIS
        // da simulação e ANTES do envio (ver execute_flashloan / execute_direct /
        // execute_wrapper). Ou seja: simula de verdade, mede a executabilidade e
        // para antes de assinar qualquer transação.
        if cfg.flashloan.enabled && cfg.execution.use_flashloan {
            if cfg.wrapper.enabled {
                return ExecutionStrategy::WrapperFlashloan;
            }
            return ExecutionStrategy::Flashloan;
        }

        ExecutionStrategy::Direct
    }

    // ========================================================================
    // EXEC DIRETA
    // ========================================================================
    pub async fn execute_direct(
        &self,
        opp: &ArbitrageOpportunity,
        slippage_bps: u64
    ) -> Result<BundleResult> {

        // CORREÇÃO: Anti-MEV não bloqueia mais
        if self.debounce_same_block().await? {
            return Ok(BundleResult::skipped()
                .with_execution_mode("same_block")
                .with_outcome(ExecutionOutcome::SameBlockRejected { tx_hash: None }));
        }

        let (dry, simulate, asset, amount, steps, flashloan_decimals, min_profit_raw) = {
            let cfg = self.config.lock().await;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;

            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped()
                    .with_execution_mode("complexity_reject")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: "route complexity filter reject".into(),
                    }));
            }

            let decimals = cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32;
            let min_profit_raw = match self.minimum_profit_raw(opp, decimals, &cfg) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "MissingProfitTokenPrice — abort Direct");
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("missing_token_price")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: "missing profit token price".into(),
                        }));
                }
            };

            (
                cfg.execution.dry_run,
                // Quote de cada hop é por notional; rota completa precisa eth_call
                // sequencial antes de enviar, mesmo se flag foi desligada.
                cfg.flashloan.simulate_before_execute.unwrap_or(true) || steps.len() > 1,
                asset,
                amount,
                steps,
                decimals,
                min_profit_raw,
            )
        };

        // APPROVE
        if !dry {
            if let Err(e) = self.approve_token_for_execution(asset, self.executor.address(), amount).await {
                warn!("❌ Approve falhou: {}", e);
                return Ok(BundleResult::skipped()
                    .with_execution_mode("approve_failed")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: format!("approve failed: {e}"),
                    }));
            }
        }

        let direct = self.executor.execute_direct(asset, amount, steps.clone(), min_profit_raw);

        if simulate {
            info!("🔬 Simulando Direct Arbitrage...");
            match self.simulate_transaction(&direct).await {
                Ok(_) => info!("✅ Simulação Direct: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Direct falhou: {}", e);
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("direct_sim_failed")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: format!("direct sim failed: {e}"),
                        }));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(direct, opp, "direct", flashloan_decimals).await
    }

    // ========================================================================
    // FLASHLOAN
    // ========================================================================
    pub async fn execute_flashloan(
        &self,
        opp: &ArbitrageOpportunity,
        slippage_bps: u64
    ) -> Result<BundleResult> {

        // CORREÇÃO: Anti-MEV não bloqueia mais
        if self.debounce_same_block().await? {
            return Ok(BundleResult::skipped()
                .with_execution_mode("same_block")
                .with_outcome(ExecutionOutcome::SameBlockRejected { tx_hash: None }));
        }

        let (dry, simulate, asset, amount, steps, flashloan_decimals, min_profit_raw) = {
            let cfg = self.config.lock().await;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;

            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped()
                    .with_execution_mode("complexity_reject")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: "route complexity filter reject".into(),
                    }));
            }

            let decimals = cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32;
            let min_profit_raw = match self.minimum_profit_raw(opp, decimals, &cfg) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "MissingProfitTokenPrice — abort Flashloan");
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("missing_token_price")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: "missing profit token price".into(),
                        }));
                }
            };

            (
                cfg.execution.dry_run,
                // Valida output real encadeado da rota antes de broadcast.
                cfg.flashloan.simulate_before_execute.unwrap_or(true) || steps.len() > 1,
                asset,
                amount,
                steps,
                decimals,
                min_profit_raw,
            )
        };

        let call = self.executor.execute_flashloan(asset, amount, steps, min_profit_raw);

        if simulate {
            info!("🔬 Simulando Flashloan...");
            match self.simulate_bool_transaction(&call).await {
                Ok(_) => info!("✅ Simulação Flashloan: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Flashloan falhou: {}", e);
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("flashloan_sim_failed")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: format!("flashloan sim failed: {e}"),
                        }));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(call, opp, "flashloan", flashloan_decimals).await
    }

    // ========================================================================
    // WRAPPER FLASHLOAN
    // ========================================================================
    pub async fn execute_wrapper(
        &self,
        opp: &ArbitrageOpportunity,
        slippage_bps: u64
    ) -> Result<BundleResult> {

        // CORREÇÃO: Anti-MEV não bloqueia mais
        if self.debounce_same_block().await? {
            return Ok(BundleResult::skipped()
                .with_execution_mode("same_block")
                .with_outcome(ExecutionOutcome::SameBlockRejected { tx_hash: None }));
        }

        let (dry, simulate, wrapper_addr, asset, amount, steps, flashloan_decimals, min_profit_raw, profit_recipient) = {
            let cfg = self.config.lock().await;

            let wrapper_addr = Address::from_str(&cfg.wrapper.address)?;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;

            if let Err(e) = self.validate_wrapper_steps(&steps, asset) {
                warn!("❌ Steps inválidos para wrapper: {}", e);
                return Ok(BundleResult::skipped()
                    .with_execution_mode("wrapper_invalid_steps")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: format!("wrapper invalid steps: {e}"),
                    }));
            }

            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped()
                    .with_execution_mode("complexity_reject")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: "route complexity filter reject".into(),
                    }));
            }

            let decimals = cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32;
            let min_profit_raw = match self.minimum_profit_raw(opp, decimals, &cfg) {
                Ok(v) => v,
                Err(e) => {
                    warn!(error = %e, "MissingProfitTokenPrice — abort Wrapper");
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("missing_token_price")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: "missing profit token price".into(),
                        }));
                }
            };
            let profit_recipient = self.resolve_profit_recipient(&cfg)?;

            (
                cfg.execution.dry_run,
                // Wrapper também executa hops encadeados; nunca pular preflight.
                cfg.flashloan.simulate_before_execute.unwrap_or(true) || steps.len() > 1,
                wrapper_addr,
                asset,
                amount,
                steps,
                decimals,
                min_profit_raw,
                profit_recipient,
            )
        };

        // CallbackData.profitRecipient — must equal on-chain owner (Q1).
        // Never encode the executor contract address as recipient.
        if let Err(e) = self
            .assert_profit_recipient_matches_owner(profit_recipient)
            .await
        {
            warn!(error = %e, "wrapper abort: recipient != owner");
            return Ok(BundleResult::skipped()
                .with_execution_mode("profit_recipient_mismatch")
                .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                    reason: format!("profit recipient mismatch: {e}"),
                }));
        }

        let params = Self::encode_callback_data(profit_recipient, &steps, min_profit_raw);

        let contract = FlashloanCaller::new(wrapper_addr, self.middleware.clone());
        let call = contract.trigger_flashloan(asset, amount, params);

        if simulate {
            info!("🔬 Simulando Wrapper Flashloan...");
            match self.simulate_transaction(&call).await {
                Ok(_) => info!("✅ Simulação Wrapper: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Wrapper falhou: {}", e);
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("wrapper_sim_failed")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: format!("wrapper sim failed: {e}"),
                        }));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(call, opp, "wrapper", flashloan_decimals).await
    }

    /// Valida steps para wrapper (igual contrato)
    fn validate_wrapper_steps(&self, steps: &[AbiSwapStep], asset: Address) -> Result<(), FlashloanError> {
        if steps.is_empty() {
            return Err(FlashloanError::InvalidRoute("Empty steps for wrapper".into()));
        }
        
        if steps[0].token_in != asset {
            return Err(FlashloanError::InvalidRoute(
                format!("First step token_in {:?} != flashloan asset {:?}", 
                       steps[0].token_in, asset)
            ));
        }
        
        if steps[steps.len() - 1].token_out != asset {
            return Err(FlashloanError::InvalidRoute(
                format!("Last step token_out {:?} != flashloan asset {:?}", 
                       steps[steps.len() - 1].token_out, asset)
            ));
        }
        
        Ok(())
    }

    // ========================================================================
    // APPROVE
    // ========================================================================
    async fn approve_token_for_execution(&self, token_addr: Address, spender: Address, amount: U256) -> Result<()> {
        {
            let cfg = self.config.lock().await;
            if paper_validation::sends_forbidden(&cfg) {
                warn!("🚫 APPROVE/SEND BLOCKED (paper/dry_run_only)");
                return Ok(());
            }
        }

        let wallet_addr = self.get_wallet_address()?;
        let token_contract = ERC20::new(token_addr, self.middleware.clone());

        let allowance: U256 = token_contract.allowance(wallet_addr, spender).call().await
            .context("Failed to get allowance")?;
        
        if allowance >= amount {
            return Ok(());
        }

        info!("🔓 Approving {:?} for spender {:?}", token_addr, spender);
        let call = token_contract.approve(spender, U256::MAX);
        let pending = call.send().await.context("Failed to send approve")?;
        
        match timeout(Duration::from_secs(30), pending).await {
            Ok(Ok(Some(receipt))) => {
                if receipt.status == Some(1.into()) {
                    info!("✅ Approved");
                    Ok(())
                } else {
                    Err(anyhow!("Approve tx reverted"))
                }
            }
            Ok(Err(e)) => Err(anyhow!("Approve confirmation failed: {}", e)),
            Err(_) => Err(anyhow!("Approve timeout")),
            _ => Err(anyhow!("Approve receipt missing")),
        }
    }

    fn get_wallet_address(&self) -> Result<Address> {
        Ok(self.middleware.address())
    }

    // ========================================================================
    // SIMULAÇÃO
    // ========================================================================
    async fn simulate_transaction<T: Detokenize>(
        &self, 
        call: &ContractCall<AppMiddleware, T>
    ) -> Result<()> {
        // Simular em `pending` inclui swaps concorrentes que o RPC já viu.
        // Se o nó não puder dar esta garantia, falhar fechado evita broadcast
        // otimista de uma rota que provavelmente reverteria.
        let pending = call.clone().block(BlockNumber::Pending);
        match timeout(Duration::from_secs(10), pending.call()).await {
            Ok(Ok(_)) => Ok(()),
            Ok(Err(e)) => {
                let error_msg = self.decode_revert_reason(&e.to_string());
                Err(anyhow!("Simulation failed: {}", error_msg))
            },
            Err(_) => Err(anyhow!("Simulation timeout")),
        }
    }
    /// Simulacao especializada para chamadas que retornam bool.
    ///
    /// O contrato FlashloanExecutor.executeFlashloan() usa try/catch e retorna
    /// false em vez de reverter quando o flashloan falha. A simulacao generica
    /// (simulate_transaction) descarta o valor de retorno (Ok(Ok(_))), tratando
    /// false como sucesso - falso positivo garantido.
    ///
    /// Este metodo inspeciona o bool: false = falha de iniciacao do flashloan.
    ///
    /// Após a correção do contrato (remoção do try/catch em executeOperation),
    /// falhas da arbitragem interna agora propagam como revert — o braço
    /// Ok(Err(e)) abaixo captura isso. Ou seja:
    ///   Ok(Ok(true))  = flashloan iniciou E arbitragem lucrou
    ///   Ok(Ok(false)) = flashloan não iniciou (falha na Aave)
    ///   Ok(Err(e))    = arbitragem falhou (swap revert, slippage, não lucrativo)
    async fn simulate_bool_transaction(
        &self,
        call: &ContractCall<AppMiddleware, bool>,
    ) -> Result<()> {
        let pending = call.clone().block(BlockNumber::Pending);
        match timeout(Duration::from_secs(10), pending.call()).await {
            Ok(Ok(true)) => Ok(()),
            Ok(Ok(false)) => Err(anyhow!(
                "Simulation failed: contract returned false (flashloan initiation failed)"
            )),
            Ok(Err(e)) => {
                let error_msg = self.decode_revert_reason(&e.to_string());
                Err(anyhow!("Simulation failed: {}", error_msg))
            },
            Err(_) => Err(anyhow!("Simulation timeout")),
        }
    }

    fn decode_revert_reason(&self, err: &str) -> String {
        paper_validation::decode_revert_message(err)
    }

    // ========================================================================
    // Envio transação
    // ========================================================================
    async fn send_and_confirm_transaction<T: Detokenize + Send + 'static>(
        &self,
        mut call: ContractCall<AppMiddleware, T>,
        opp: &ArbitrageOpportunity,
        mode: &'static str,
        flashloan_decimals: u32,
    ) -> Result<BundleResult> {

        // HARD GATE: paper / dry_run_only / dry_run — fisicamente impossível broadcast.
        {
            let cfg = self.config.lock().await;
            if paper_validation::sends_forbidden(&cfg) {
                warn!(
                    "🚫 SEND BLOCKED mode={} (paper/dry_run_only/dry_run) — nenhum broadcast",
                    mode
                );
                return Ok(BundleResult::skipped().with_execution_mode("paper_send_blocked"));
            }
        }

        // FASE 6 — relay privado obrigatório: se o operador exigir mempool privado
        // e o caminho de envio via ExecutionEngine/BundleSender não estiver ativo,
        // aborta pré-broadcast (fail-closed). Nunca cai no mempool público quando
        // o relay é requisitado. (O routing via BundleSender é o ponto pendente;
        // até estar wired, private_relay_required=true aborta todas as sends.)
        {
            let cfg = self.config.lock().await;
            if cfg.mev.private_relay_required {
                let routing_active = cfg.mev.enabled && self.execution_engine.is_some();
                if !routing_active {
                    warn!(
                        "🚫 ABORT pre-broadcast: private_relay_required=true mas routing via \
                         BundleSender inativo (mev.enabled={}, engine={})",
                        cfg.mev.enabled,
                        self.execution_engine.is_some()
                    );
                    return Ok(BundleResult::skipped()
                        .with_execution_mode("private_relay_unavailable")
                        .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                            reason: "private relay required but BundleSender routing inactive"
                                .into(),
                        }));
                }
            }
            if !cfg.mev.enabled {
                info!(
                    "⚠️ PublicMempoolDegraded: enviando via mempool público (relay privado off). \
                     private_relay_required={}",
                    cfg.mev.private_relay_required
                );
            }
        }

        // Política de retry/RBF. Uma tentativa lógica = um nonce. Retry = replacement
        // (mesmo nonce + gas maior), nunca nonce novo. Evita double execution: duas
        // txs com nonces distintos que ambas incluem e ambas executam a arbitragem.
        let (max_retries, replace_multiplier, confirm_timeout) = {
            let cfg = self.config.lock().await;
            let m = cfg.execution.replace_multiplier.unwrap_or(1.12).max(1.0);
            // max_retries limitado a 4 p/ não virar spam de replacement.
            let retries = cfg.execution.max_retries.clamp(1, 4) as usize;
            (retries, m, Duration::from_secs(30))
        };
        let send_timeout = Duration::from_secs(10);
        let multiplier_bps = U256::from((replace_multiplier * 100.0).round() as u64);

        // Reserva UM nonce para a tentativa lógica inteira. Buscar pending nonce
        // aqui (uma vez) e fixá-lo no TypedTransaction faz ethers não re-fetch
        // nonce em cada call.send() — que era a raiz do bug double-execution.
        let sender = self.get_wallet_address()?;
        let nonce = match self
            .middleware
            .get_transaction_count(sender, Some(BlockId::Number(BlockNumber::Pending)))
            .await
        {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "nonce fetch falhou — abort pre-broadcast");
                return Ok(BundleResult::skipped()
                    .with_execution_mode("nonce_fetch_failed")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: format!("nonce fetch failed: {e}"),
                    }));
            }
        };

        let (mut max_fee, mut max_priority) = {
            let mut tx_req = Eip1559TransactionRequest::new();
            self.gas_estimator.populate_dynamic_gas(&mut tx_req).await?
        };

        let opp_id = opp.id.clone();
        let mut latest_tx_hash: Option<H256> = None;
        let mut replacement_count: u32 = 0;

        for attempt in 0..max_retries {
            if attempt > 0 {
                // Bump integer (x100 → /100) p/ evitar drift de f64. Mesmo nonce.
                max_fee = max_fee.saturating_mul(multiplier_bps) / U256::from(100u64);
                max_priority =
                    max_priority.saturating_mul(multiplier_bps) / U256::from(100u64);
                replacement_count += 1;
                info!(
                    opp_id = %opp_id, mode, attempt, replacement_count,
                    nonce = ?nonce, max_fee = ?max_fee, max_priority = ?max_priority,
                    "🔄 RBF replacement: mesmo nonce, gas bumped"
                );
            }

            // Fixa nonce + gas no TypedTransaction do ContractCall. ethers skipa
            // o fetch de nonce em send() quando o campo já está setado.
            if let Some(tx) = call.tx.as_eip1559_mut() {
                tx.nonce = Some(nonce);
                tx.max_fee_per_gas = Some(max_fee);
                tx.max_priority_fee_per_gas = Some(max_priority);
            } else {
                // Policy exige EIP-1559. Tx legada = abort fail-closed.
                return Ok(BundleResult::skipped()
                    .with_execution_mode("non_eip1559_abort")
                    .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                        reason: "tx não é EIP-1559 — abort fail-closed".into(),
                    }));
            }

            info!(
                opp_id = %opp_id, mode, attempt = attempt + 1, max_retries,
                nonce = ?nonce, max_fee = ?max_fee, max_priority = ?max_priority,
                "💸 Enviando TX"
            );

            match timeout(send_timeout, call.send()).await {
                Ok(Ok(pending)) => {
                    let tx_hash = pending.tx_hash();
                    latest_tx_hash = Some(tx_hash);
                    info!(
                        opp_id = %opp_id, nonce = ?nonce, tx_hash = ?tx_hash,
                        "⏳ TX enviada — aguardando confirmação"
                    );

                    match timeout(confirm_timeout, pending).await {
                        Ok(Ok(Some(receipt))) => {
                            let r_hash = receipt.transaction_hash;
                            if receipt.status == Some(1.into()) {
                                info!(opp_id = %opp_id, tx_hash = ?r_hash, "✅ TX confirmada");
                                // Anti-MEV: registra bloco só após execução confirmada.
                                self.update_execution_block().await;
                                if let Some(gas_used) = receipt.gas_used {
                                    let gas_kind = if mode == "direct" {
                                        GasStrategyKind::Direct
                                    } else {
                                        GasStrategyKind::WithFlashloan
                                    };
                                    self.gas_estimator
                                        .observe_gas_used(opp.steps.0.len().max(1), gas_used, gas_kind)
                                        .await;
                                }
                                let contract_profit = self
                                    .extract_real_profit_from_receipt(&receipt, opp, flashloan_decimals)
                                    .unwrap_or_else(|| {
                                        warn!("receipt sem evento de lucro; PnL será limite inferior");
                                        0.0
                                    });
                                let real_profit = self
                                    .realized_net_profit_after_gas_usd(&receipt, contract_profit)
                                    .await;
                                metrics::inc_arbitrage_executions();
                                metrics::inc_exec_ok();
                                metrics::set_last_profit(real_profit);
                                let outcome = if real_profit >= 0.0 {
                                    ExecutionOutcome::ConfirmedProfit {
                                        tx_hash: r_hash,
                                        realized_profit_usd: real_profit,
                                        gas_used: receipt.gas_used.unwrap_or_default(),
                                    }
                                } else {
                                    ExecutionOutcome::ConfirmedLoss {
                                        tx_hash: r_hash,
                                        realized_loss_usd: -real_profit,
                                        gas_used: receipt.gas_used.unwrap_or_default(),
                                    }
                                };
                                return Ok(BundleResult::new(true, real_profit, opp.gas_cost_usd)
                                    .with_execution_mode(mode)
                                    .with_tx_hash(Some(format!("{:?}", r_hash)))
                                    .with_outcome(outcome));
                            } else {
                                // Revert: tx minerada com status 0. NUNCA skip.
                                warn!(opp_id = %opp_id, tx_hash = ?r_hash, "❌ TX revertida");
                                return Ok(BundleResult::skipped()
                                    .with_execution_mode("tx_reverted")
                                    .with_tx_hash(Some(format!("{:?}", r_hash)))
                                    .with_outcome(ExecutionOutcome::Reverted {
                                        tx_hash: r_hash,
                                        reason: None,
                                        gas_used: receipt.gas_used,
                                    }));
                            }
                        }
                        Ok(Ok(None)) => {
                            // Receipt None: tx pode estar pending. Próximo attempt =
                            // replacement mesmo nonce. Último attempt → TimeoutStuck.
                            warn!(opp_id = %opp_id, nonce = ?nonce, "⏰ receipt None — tx possivelmente pending");
                            if attempt + 1 >= max_retries {
                                return Ok(self.timeout_stuck_result(&opp_id, nonce, latest_tx_hash));
                            }
                            continue;
                        }
                        Ok(Err(e)) => {
                            warn!(opp_id = %opp_id, nonce = ?nonce, error = %e, "⏰ erro na confirmação");
                            if attempt + 1 >= max_retries {
                                return Ok(self.timeout_stuck_result(&opp_id, nonce, latest_tx_hash));
                            }
                            continue;
                        }
                        Err(_) => {
                            warn!(opp_id = %opp_id, nonce = ?nonce, "⏰ timeout na confirmação");
                            if attempt + 1 >= max_retries {
                                return Ok(self.timeout_stuck_result(&opp_id, nonce, latest_tx_hash));
                            }
                            continue;
                        }
                    }
                }
                Ok(Err(e)) => {
                    let s = e.to_string().to_lowercase();
                    // Sinais de que a tx já está na rede (nonce low, already known,
                    // replacement underpriced, known transaction): não recriar com nonce
                    // novo. Último attempt → TimeoutStuck (assume pendente). Caso
                    // contrário → Dropped (nonce liberado p/ reuso futuro).
                    let already_known = s.contains("nonce")
                        || s.contains("already known")
                        || s.contains("replacement")
                        || s.contains("known transaction");
                    if attempt + 1 >= max_retries {
                        let (mode_str, outcome) = if already_known {
                            (
                                "timeout_stuck",
                                ExecutionOutcome::TimeoutStuck { nonce, latest_tx_hash },
                            )
                        } else {
                            ("dropped", ExecutionOutcome::Dropped { nonce })
                        };
                        warn!(
                            opp_id = %opp_id, nonce = ?nonce, already_known,
                            error = %e, "❌ send falhou no último attempt"
                        );
                        return Ok(BundleResult::skipped()
                            .with_execution_mode(mode_str)
                            .with_tx_hash(latest_tx_hash.map(|h| format!("{:?}", h)))
                            .with_outcome(outcome));
                    }
                    warn!(opp_id = %opp_id, nonce = ?nonce, error = %e, "❌ erro send — retry mesmo nonce");
                    continue;
                }
                Err(_) => {
                    // Timeout no envio: tx pode ter sido broadcast. Reenvio como
                    // replacement mesmo nonce (RBF) no próximo attempt.
                    warn!(opp_id = %opp_id, nonce = ?nonce, "⏰ timeout no envio — tentará replacement");
                    if attempt + 1 >= max_retries {
                        return Ok(self.timeout_stuck_result(&opp_id, nonce, latest_tx_hash));
                    }
                    continue;
                }
            }
        }

        // Loop esgotado sem return defensivo — trata como stuck (não skip genérico).
        Ok(self.timeout_stuck_result(&opp_id, nonce, latest_tx_hash))
    }

    /// Constrói BundleResult de TimeoutStuck com log estruturado único.
    fn timeout_stuck_result(
        &self,
        opp_id: &str,
        nonce: U256,
        latest_tx_hash: Option<H256>,
    ) -> BundleResult {
        warn!(
            opp_id = %opp_id, nonce = ?nonce, latest_tx_hash = ?latest_tx_hash,
            outcome = "TimeoutStuck", "⏰ tentativa lógica esgotada — nonce possivelmente pendente, NÃO reusar"
        );
        BundleResult::skipped()
            .with_execution_mode("timeout_stuck")
            .with_tx_hash(latest_tx_hash.map(|h| format!("{:?}", h)))
            .with_outcome(ExecutionOutcome::TimeoutStuck { nonce, latest_tx_hash })
    }

    /// C3: decodifica o profit REAL do evento `FlashLoanSuccess` emitido pelo
    /// `FlashloanExecutor` no receipt. Antes era stub `None` — PnL pós-execução
    /// ficava = estimativa teórica, impossibilitando calibrar economics.
    ///
    /// Evento: `FlashLoanSuccess(address indexed asset, uint256 amount,
    /// uint256 premium, uint256 profit, address indexed executor)`
    /// — `profit` está em raw units do `asset` (decimais do token emprestado,
    /// tipicamente USDT/USDC = 6). Converte p/ USD via token_price_usd do opp.
    ///
    /// Também cobre `DirectExecution` (mesma assinatura de campo profit na
    /// posição 3 dos dados não-indexados).
    fn extract_real_profit_from_receipt(
        &self,
        receipt: &TransactionReceipt,
        opp: &ArbitrageOpportunity,
        flashloan_decimals: u32,
    ) -> Option<f64> {
        use ethers::abi::ParamType;

        // Ambos eventos carregam `profit` como terceiro uint256 não-indexado.
        let flashloan_topic = H256::from(ethers::utils::keccak256(
            b"FlashLoanSuccess(address,uint256,uint256,uint256,address)",
        ));
        let direct_topic = H256::from(ethers::utils::keccak256(
            b"DirectExecution(address,uint256,uint256,address,uint256)",
        ));

        for log in receipt.logs.iter() {
            let Some(t0) = log.topics.first() else { continue };
            if (*t0 != flashloan_topic && *t0 != direct_topic)
                || log.address != self.executor.address()
            {
                continue;
            }
            // Dados não-indexados: amount, premium, profit (3 × 32 bytes).
            let data = log.data.as_ref();
            if data.len() < 96 {
                warn!("FlashLoanSuccess: log data curto ({} bytes) — decode skip", data.len());
                continue;
            }
            let tokens = match ethers::abi::decode(
                &[
                    ParamType::Uint(256),
                    ParamType::Uint(256),
                    ParamType::Uint(256),
                ],
                data,
            ) {
                Ok(t) => t,
                Err(e) => {
                    warn!("FlashLoanSuccess: decode falhou: {e}");
                    continue;
                }
            };
            let profit_raw = match tokens.into_iter().nth(2) {
                Some(Token::Uint(v)) => v,
                _ => continue,
            };
            let token_price = opp.token_price_usd.unwrap_or(0.0);
            let profit_usd =
                self.token_amount_to_usd_display_f64(profit_raw, token_price, flashloan_decimals);
            info!(
                "💰 Profit real do receipt: {} raw ({} dec) → ${:.6} (price=${:.4})",
                profit_raw, flashloan_decimals, profit_usd, token_price
            );
            return Some(profit_usd);
        }
        // Sem evento FlashLoanSuccess (ex.: wrapper path emite evento diferente
        // ou TX revertedida em depth) — caller cai p/ estimated_profit_usd.
        None
    }

    /// PnL realizado da wallet = lucro transferido pelo executor menos gás de
    /// inclusão efetivamente pago. `effective_gas_price` vem do receipt, não do
    /// teto EIP-1559 nem da estimativa pré-envio.
    async fn realized_net_profit_after_gas_usd(
        &self,
        receipt: &TransactionReceipt,
        contract_profit_usd: f64,
    ) -> f64 {
        let (Some(gas_used), Some(effective_gas_price)) =
            (receipt.gas_used, receipt.effective_gas_price)
        else {
            warn!(
                "receipt sem gas_used/effective_gas_price; PnL mantém lucro do contrato=${:.6}",
                contract_profit_usd
            );
            return contract_profit_usd;
        };

        let gas_wei = gas_used.saturating_mul(effective_gas_price);
        let gas_pol = u256_to_f64(gas_wei, 18);
        let pol_usd = crate::infra::price_feed::PRICE_FEED
            .get_price("WMATIC")
            .await
            .unwrap_or_else(|_| crate::infra::price_feed::CachedPriceFeed::fallback_price("WMATIC"));
        let gas_usd = gas_pol * pol_usd;
        let net = contract_profit_usd - gas_usd;
        info!(
            "💵 PnL realizado: lucro_contrato=${:.6} - gas={} wei (${:.6}) = net=${:.6}",
            contract_profit_usd, gas_wei, gas_usd, net
        );
        net
    }

    // ========================================================================
    // CONVERSÃO DE STEPS
    // ========================================================================
    fn extract_and_convert_opp_data(
        &self,
        opp: &ArbitrageOpportunity,
        cfg: &Config,
        _slippage_bps: u64,
    ) -> Result<(Address, U256, Vec<AbiSwapStep>)> {

        // M13: `USDC` e `USDC.e` são contratos distintos. Sanitiza com a
        // identidade de endereço resolvida da config, não pelo ticker.
        let steps = ArbitrageEngine::sanitize_steps_with_token_identity(&opp.steps.0, |symbol| {
            self.get_token_addr(symbol, cfg)
                .ok()
                .map(|address| format!("{address:#x}"))
        });
        let first = steps.first().context("Steps vazios")?;

        let token_in = self.get_token_addr(&first.token_in, cfg)?;
        let amount_in = opp.amount_in;

        let mut abi_steps = vec![];

        for s in steps {
            // A6: amount_out_min já inclui slippage+safety em
            // `ArbitrageEngine::apply_slippage_safe` — NÃO deduzir de novo.
            let amount_out_min = s.amount_out_min;
            let extra_data = Self::build_extra_data_for_step(&s)?;

            abi_steps.push(AbiSwapStep {
                dex_type: self.map_dex_type(&s.dex_name)?,
                token_in: self.get_token_addr(&s.token_in, cfg)?,
                token_out: self.get_token_addr(&s.token_out, cfg)?,
                amount_out_min,
                extra_data,
            });
        }

        info!(
            "🔄 Rota convertida: {} steps | Amount: {} | slippage único no engine",
            abi_steps.len(),
            amount_in
        );

        Ok((token_in, amount_in, abi_steps))
    }

    // ========================================================================
    // MAP DEX
    // ========================================================================
    fn map_dex_type(&self, dex: &str) -> Result<u8> {
        let normalized = dex.to_lowercase()
            .replace(" ", "")
            .replace("_", "")
            .replace("v2", "")
            .replace("v3", "");
        
        match normalized.as_str() {
            "quickswap" => Ok(0),
            "sushiswap" => Ok(1),
            "uniswap" => Ok(2),
            _ => {
                warn!("⚠️ DEX não mapeada: '{}' (normalizada: '{}')", dex, normalized);
                Err(anyhow!("DEX não suportada: {}", dex))
            }
        }
    }

    fn get_token_addr(&self, symbol: &str, cfg: &Config) -> Result<Address> {
        if let Some(addr) = cfg.addresses.get(symbol) {
            return Ok(*addr);
        }

        let s = cfg.pairs.tokens
            .get(symbol)
            .ok_or_else(|| anyhow!("Token não encontrado: {}", symbol))?;

        Ok(Address::from_str(s)?)
    }
}

// ============================================================================
// TESTES — src/core/flashloan.rs
// ----------------------------------------------------------------------------
// ArbitrageClient usa AppMiddleware concreto (não trait). Os métodos puros
// abaixo (que não tocam middleware) são testados construindo um client com
// provider/wallet dummy. Métodos async que chamam middleware/contract não são
// cobertos aqui (precisariam de fork anvil + RPC real, fora do escopo).
// ============================================================================
#[cfg(test)]
mod tests {
    use super::*;

    use ethers::providers::Provider;
    use ethers::signers::LocalWallet;
    use std::str::FromStr;
    use crate::infra::rotating_http_client::RotatingHttpClient;

    // Chave de teste hardhat account #0 — bem conhecida, sem valor.
    const TEST_PK: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Constrói um ArbitrageClient com middleware dummy (RPC localhost).
    /// Nenhum método testado abaixo realiza chamadas de rede. Usa
    /// RotatingHttpClient (mesmo tipo de transporte do AppMiddleware em
    /// produção) — antes usava Provider<Http>, que não casa com o tipo
    /// atual de AppMiddleware e quebrava a compilação dos testes.
    fn make_client() -> ArbitrageClient {
        let transport = RotatingHttpClient::from_strings(
            &["http://127.0.0.1:8545".to_string()],
            std::time::Duration::from_secs(5),
        )
        .expect("transporte dummy válido");
        let provider = Provider::new(transport);
        let provider = Arc::new(provider);
        let wallet = LocalWallet::from_str(TEST_PK).expect("PK teste válida");
        let middleware = Arc::new(AppMiddleware::new(provider, wallet));
        let config = Arc::new(Mutex::new(Config::default()));
        ArbitrageClient::new(Address::zero(), middleware, config, None)
    }

    /// Helper: constrói um AbiSwapStep com extra_data vazio.
    fn step(dex: u8, token_in: Address, token_out: Address, amount_out_min: U256) -> AbiSwapStep {
        AbiSwapStep {
            dex_type: dex,
            token_in,
            token_out,
            amount_out_min,
            extra_data: Bytes::new(),
        }
    }

    // ------------------------------------------------------------------------
    // calculate_flashloan_fee — config fee_pct (5 bps = 0.0005)
    // ------------------------------------------------------------------------
    #[test]
    fn flashloan_fee_is_five_bps() {
        let client = make_client();
        // 1e18 * 5 / 10000 = 5e14
        let amount = U256::from(10).pow(U256::from(18));
        let fee = client.calculate_flashloan_fee(amount, 0.0005);
        assert_eq!(fee, U256::from(500_000_000_000_000u64));
        assert_eq!(fee * U256::from(10000), amount * U256::from(5));
    }

    #[test]
    fn flashloan_fee_zero_amount() {
        let client = make_client();
        assert_eq!(
            client.calculate_flashloan_fee(U256::zero(), 0.0005),
            U256::zero()
        );
    }

    #[test]
    fn flashloan_fee_not_nine_bps() {
        let client = make_client();
        let amount = U256::from(10).pow(U256::from(18));
        let fee5 = client.calculate_flashloan_fee(amount, 0.0005);
        let fee9 = amount * U256::from(9) / U256::from(10000);
        assert_ne!(fee5, fee9, "não deve mais usar 9 bps hardcoded");
    }

    // ------------------------------------------------------------------------
    // apply_slippage
    // ------------------------------------------------------------------------
    // ------------------------------------------------------------------------
    // extract_real_profit_from_receipt — C3
    // ------------------------------------------------------------------------
    fn flashloan_success_log(profit_raw: U256) -> ethers::types::Log {
        use ethers::types::{Bytes, H256, Log};
        let topic0 = H256::from(ethers::utils::keccak256(
            b"FlashLoanSuccess(address,uint256,uint256,uint256,address)",
        ));
        // Dados não-indexados: amount, premium, profit (cada 32 bytes, padded).
        let data = ethers::abi::encode(&[
            Token::Uint(U256::from(1_000_000)), // amount
            Token::Uint(U256::from(500)),       // premium
            Token::Uint(profit_raw),            // profit
        ]);
        Log {
            address: Address::zero(),
            topics: vec![topic0],
            data: Bytes::from(data),
            ..Default::default()
        }
    }

    fn direct_execution_log(profit_raw: U256) -> ethers::types::Log {
        use ethers::types::{Bytes, H256, Log};
        let topic0 = H256::from(ethers::utils::keccak256(
            b"DirectExecution(address,uint256,uint256,address,uint256)",
        ));
        let data = ethers::abi::encode(&[
            Token::Uint(U256::from(1_000_000)),
            Token::Uint(U256::from(1_100_000)),
            Token::Uint(profit_raw),
        ]);
        Log {
            address: Address::zero(),
            topics: vec![topic0],
            data: Bytes::from(data),
            ..Default::default()
        }
    }

    #[test]
    fn extract_real_profit_decodes_flashloan_success_event() {
        let client = make_client();
        // profit = 1.5 USDT (6 decimais) = 1_500_000 raw, preço USDT ≈ $1
        let profit_raw = U256::from(1_500_000u64);
        let log = flashloan_success_log(profit_raw);
        let receipt = TransactionReceipt {
            logs: vec![log],
            ..Default::default()
        };
        let mut opp = ArbitrageOpportunity::default();
        opp.token_price_usd = Some(1.0);
        let profit_usd = client
            .extract_real_profit_from_receipt(&receipt, &opp, 6)
            .expect("deve decodificar profit");
        assert!((profit_usd - 1.5).abs() < 1e-9, "profit_usd={}", profit_usd);
    }

    #[test]
    fn extract_real_profit_returns_none_when_no_event() {
        let client = make_client();
        let receipt = TransactionReceipt::default();
        let opp = ArbitrageOpportunity::default();
        assert!(client.extract_real_profit_from_receipt(&receipt, &opp, 6).is_none());
    }

    #[test]
    fn extract_real_profit_decodes_direct_execution_event() {
        let client = make_client();
        let receipt = TransactionReceipt {
            logs: vec![direct_execution_log(U256::from(250_000u64))],
            ..Default::default()
        };
        let mut opp = ArbitrageOpportunity::default();
        opp.token_price_usd = Some(1.0);
        assert_eq!(
            client.extract_real_profit_from_receipt(&receipt, &opp, 6),
            Some(0.25)
        );
    }

    #[test]
    fn apply_slippage_reduces_by_bps() {
        let client = make_client();
        let amount = U256::from(1_000_000);
        // 50 bps => 995000
        assert_eq!(client.apply_slippage(amount, 50), U256::from(995_000));
    }

    #[test]
    fn apply_slippage_zero_bps_noop() {
        let client = make_client();
        let amount = U256::from(1_000_000);
        assert_eq!(client.apply_slippage(amount, 0), amount);
    }

    #[test]
    fn apply_slippage_full_amount_bps() {
        let client = make_client();
        // 10000 bps = 100% => 0
        assert_eq!(client.apply_slippage(U256::from(1_000_000), 10000), U256::zero());
    }

    // ------------------------------------------------------------------------
    // token_amount_to_usd_display_f64 (telemetry)
    // ------------------------------------------------------------------------
    #[test]
    fn token_amount_to_usd_6_decimals() {
        let client = make_client();
        // 1_000_000 units @ 6 decimals, price $1 => $1.00
        let usd = client.token_amount_to_usd_display_f64(U256::from(1_000_000), 1.0, 6);
        assert!((usd - 1.0).abs() < 1e-9, "usd={usd}");
    }

    #[test]
    fn token_amount_to_usd_18_decimals() {
        let client = make_client();
        // 1e18 units @ 18 decimals, price $2 => $2.00
        let amount = U256::from(10).pow(U256::from(18));
        let usd = client.token_amount_to_usd_display_f64(amount, 2.0, 18);
        assert!((usd - 2.0).abs() < 1e-9, "usd={usd}");
    }

    // ------------------------------------------------------------------------
    // validate_profit_after_fees (A5 gross-based, uma dedução)
    // ------------------------------------------------------------------------
    #[test]
    fn profit_validation_ok_when_net_positive() {
        let client = make_client();
        // gross $5 - gas $0.50 - fee $0.50 => net $4.00
        let res = client.validate_profit_after_fees(5.0, 0.50, 0.50, 0.0);
        assert!(res.is_ok(), "deveria aceitar profit positivo: {:?}", res);
    }

    #[test]
    fn profit_validation_rejects_net_zero_or_negative() {
        let client = make_client();
        // gross $1 - gas $0.50 - fee $0.60 => net -$0.10
        let res = client.validate_profit_after_fees(1.0, 0.50, 0.60, 0.0);
        let err = res.expect_err("deveria rejeitar net negativo");
        assert!(matches!(err, FlashloanError::InsufficientProfit(_)));
        assert!(err.to_string().contains("Net profit"));
    }

    #[test]
    fn profit_validation_single_deduction_no_double() {
        let client = make_client();
        // gross 10, gas 1, fee 0.5 => net exatamente 8.5 (uma vez)
        let gross = 10.0_f64;
        let gas = 1.0_f64;
        let fee = 0.5_f64;
        assert!(client.validate_profit_after_fees(gross, gas, fee, 0.0).is_ok());
        let expected_net = gross - gas - fee;
        assert!((expected_net - 8.5).abs() < 1e-12);
        // Se subtraísse de novo (double), net ficaria 8.5 - 1 - 0.5 = 7.0 — gate
        // atual NÃO faz isso: basta gross - gas - fee > 0.
        assert!(expected_net > 7.0);
    }

    #[test]
    fn profit_validation_accepts_when_engine_net_already_positive() {
        // Simula: engine já filtrou net>min; validate usa GROSS e só gas+fee.
        // gross=2, gas=0.1, fee_usd(5bps on $100)=0.05 => net 1.85 > 0
        let client = make_client();
        let amount = U256::from(100_000_000u64); // 100 USDC 6dec
        let fee_tok = client.calculate_flashloan_fee(amount, 0.0005);
        let fee_usd = client.token_amount_to_usd_display_f64(fee_tok, 1.0, 6);
        assert!((fee_usd - 0.05).abs() < 1e-9, "fee_usd={fee_usd}");
        assert!(client.validate_profit_after_fees(2.0, 0.1, fee_usd, 0.0).is_ok());
    }

    // ------------------------------------------------------------------------
    // A4 — extraData V3 encode / abort
    // ------------------------------------------------------------------------
    #[test]
    fn encode_v3_fee_extra_data_roundtrip() {
        // ABI encode/decode aceita qualquer uint24; A4 filtra o que vai ao executor.
        for fee in [100u32, 500, 3000, 10_000] {
            let encoded = ArbitrageClient::encode_v3_fee_extra_data(fee);
            assert!(!encoded.is_empty());
            let decoded = ArbitrageClient::decode_v3_fee_extra_data(&encoded).unwrap();
            assert_eq!(decoded, fee, "round-trip fee={fee}");
        }
        for fee in crate::dex::EXECUTABLE_V3_FEE_TIERS {
            assert!(crate::dex::is_executable_v3_fee_tier(fee));
        }
    }

    #[test]
    fn executable_route_extra_data_never_fee100() {
        use crate::core::types::ArbitrageStep;
        use crate::dex::select_executable_v3_best_out;

        let quotes = [
            (100u32, U256::from(5_000u64)),
            (500u32, U256::from(4_000u64)),
            (3000u32, U256::from(3_000u64)),
        ];
        let (fee, _) = select_executable_v3_best_out(&quotes).unwrap();
        assert_ne!(fee, 100);
        let step = ArbitrageStep {
            dex_name: "UniswapV3".into(),
            token_in: "A".into(),
            token_out: "B".into(),
            expected_rate: 1.0,
            amount_out_min: U256::from(1),
            v3_fee_tier: Some(fee),
            ..Default::default()
        };
        let extra = ArbitrageClient::build_extra_data_for_step(&step).unwrap();
        let decoded = ArbitrageClient::decode_v3_fee_extra_data(&extra).unwrap();
        assert!(matches!(decoded, 500 | 3000 | 10_000));
    }

    #[test]
    fn v2_and_curve_extra_data_empty() {
        use crate::core::types::ArbitrageStep;
        for dex in ["QuickSwap", "SushiSwap", "Curve"] {
            let step = ArbitrageStep {
                dex_name: dex.into(),
                token_in: "USDT".into(),
                token_out: "WMATIC".into(),
                expected_rate: 1.0,
                amount_out_min: U256::from(1),
                ..Default::default()
            };
            let extra = ArbitrageClient::build_extra_data_for_step(&step).unwrap();
            assert!(extra.is_empty(), "{dex} deve ter extraData vazio");
        }
    }

    #[test]
    fn v3_extra_data_uses_cached_fee_not_silent_3000() {
        use crate::core::types::ArbitrageStep;
        use crate::dex::cache_fee_tier;

        cache_fee_tier("UniswapV3", "USDT", "WETH_A4_TEST", 500);
        let step = ArbitrageStep {
            dex_name: "UniswapV3".into(),
            token_in: "USDT".into(),
            token_out: "WETH_A4_TEST".into(),
            expected_rate: 1.0,
            amount_out_min: U256::from(1),
            v3_fee_tier: Some(500),
            ..Default::default()
        };
        let extra = ArbitrageClient::build_extra_data_for_step(&step).unwrap();
        let decoded = ArbitrageClient::decode_v3_fee_extra_data(&extra).unwrap();
        assert_eq!(decoded, 500);
        assert_ne!(decoded, 3000);
    }

    #[test]
    fn v3_without_fee_cache_aborts() {
        use crate::core::types::ArbitrageStep;
        let step = ArbitrageStep {
            dex_name: "UniswapV3".into(),
            token_in: "NOCACHE_IN".into(),
            token_out: "NOCACHE_OUT".into(),
            expected_rate: 1.0,
            amount_out_min: U256::from(1),
            v3_fee_tier: None,
            ..Default::default()
        };
        let err = ArbitrageClient::build_extra_data_for_step(&step).unwrap_err();
        assert!(
            err.to_string().contains("abort") || err.to_string().contains("ausente"),
            "err={err}"
        );
    }

    #[test]
    fn v3_fee_100_aborts_executor_unsupported() {
        use crate::core::types::ArbitrageStep;
        let step = ArbitrageStep {
            dex_name: "UniswapV3".into(),
            token_in: "A".into(),
            token_out: "B".into(),
            expected_rate: 1.0,
            amount_out_min: U256::from(1),
            v3_fee_tier: Some(100),
            ..Default::default()
        };
        let err = ArbitrageClient::build_extra_data_for_step(&step).unwrap_err();
        assert!(err.to_string().contains("100"), "err={err}");
    }

    // ------------------------------------------------------------------------
    // validate_route_consistency
    // ------------------------------------------------------------------------
    #[test]
    fn route_consistency_ok_for_valid_cycle() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        assert!(client.validate_route_consistency(&steps).is_ok());
    }

    #[test]
    fn route_consistency_rejects_broken_chain() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            // token_in c != prev token_out b
            step(0, c, a, U256::from(99)),
        ];
        let err = client.validate_route_consistency(&steps).expect_err("cadeia quebrada");
        assert!(matches!(err, FlashloanError::InvalidRoute(_)));
    }

    #[test]
    fn route_consistency_rejects_non_cycle() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        // Quebra o ciclo: último token_out != primeiro token_in
        let mut broken = steps.clone();
        broken[1].token_out = Address::from_low_u64_be(3);
        let err = client.validate_route_consistency(&broken).expect_err("não fecha ciclo");
        assert!(err.to_string().contains("return to initial token"));
    }

    // ------------------------------------------------------------------------
    // validate_steps_critical
    // ------------------------------------------------------------------------
    #[test]
    fn steps_critical_rejects_empty() {
        let client = make_client();
        let err = client.validate_steps_critical(&[]).expect_err("steps vazios");
        assert!(matches!(err, FlashloanError::InvalidRoute(_)));
        assert!(err.to_string().contains("Empty steps"));
    }

    #[test]
    fn steps_critical_rejects_zero_amount_out_min() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::zero()), // amount_out_min zero
            step(0, b, a, U256::from(99)),
        ];
        let err = client.validate_steps_critical(&steps).expect_err("amount_out_min zero");
        assert!(err.to_string().contains("amount_out_min is zero"));
    }

    #[test]
    fn steps_critical_ok_valid_cycle() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        assert!(client.validate_steps_critical(&steps).is_ok());
    }

    // ------------------------------------------------------------------------
    // validate_route_complexity
    // ------------------------------------------------------------------------
    #[test]
    fn route_complexity_ok_within_max() {
        let client = make_client();
        let cfg = Config::default(); // max_path_length = 3
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        assert!(client.validate_route_complexity(&steps, &cfg).is_ok());
    }

    #[test]
    fn route_complexity_rejects_exceeding_max() {
        let client = make_client();
        let cfg = Config::default(); // max_path_length = 3
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let d = Address::from_low_u64_be(4);
        // 4 steps > max 3
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, c, U256::from(99)),
            step(0, c, d, U256::from(98)),
            step(0, d, a, U256::from(97)),
        ];
        let err = client.validate_route_complexity(&steps, &cfg)
            .expect_err("excede max hops");
        assert!(matches!(err, FlashloanError::RouteTooComplex(_)));
        assert!(err.to_string().contains("exceeds maximum"));
    }

    // ------------------------------------------------------------------------
    // validate_wrapper_steps
    // ------------------------------------------------------------------------
    #[test]
    fn wrapper_steps_ok_when_first_and_last_match_asset() {
        let client = make_client();
        let asset = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, asset, b, U256::from(100)),
            step(0, b, asset, U256::from(99)),
        ];
        assert!(client.validate_wrapper_steps(&steps, asset).is_ok());
    }

    #[test]
    fn wrapper_steps_rejects_first_mismatch() {
        let client = make_client();
        let asset = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, b, asset, U256::from(99)), // token_in != asset
        ];
        let err = client.validate_wrapper_steps(&steps, asset)
            .expect_err("primeiro step não casa com asset");
        assert!(err.to_string().contains("First step token_in"));
    }

    #[test]
    fn wrapper_steps_rejects_last_mismatch() {
        let client = make_client();
        let asset = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let steps = vec![
            step(0, asset, b, U256::from(100)),
            step(0, b, c, U256::from(99)), // token_out != asset
        ];
        let err = client.validate_wrapper_steps(&steps, asset)
            .expect_err("último step não fecha no asset");
        assert!(err.to_string().contains("Last step token_out"));
    }

    #[test]
    fn wrapper_steps_rejects_empty() {
        let client = make_client();
        let asset = Address::from_low_u64_be(1);
        let err = client.validate_wrapper_steps(&[], asset).expect_err("steps vazios");
        assert!(err.to_string().contains("Empty steps"));
    }

    // ------------------------------------------------------------------------
    // map_dex_type
    // ------------------------------------------------------------------------
    #[test]
    fn map_dex_type_known_dexes() {
        let client = make_client();
        assert_eq!(client.map_dex_type("QuickSwap").unwrap(), 0);
        assert_eq!(client.map_dex_type("SushiSwap").unwrap(), 1);
        assert_eq!(client.map_dex_type("Uniswap").unwrap(), 2);
    }

    #[test]
    fn map_dex_type_normalizes_variants() {
        let client = make_client();
        // normalização remove espaços/underscores/v2/v3
        assert_eq!(client.map_dex_type("Quick Swap").unwrap(), 0);
        assert_eq!(client.map_dex_type("Quick_Swap").unwrap(), 0);
        assert_eq!(client.map_dex_type("UniswapV3").unwrap(), 2);
        assert_eq!(client.map_dex_type("Uniswap V2").unwrap(), 2);
    }

    #[test]
    fn map_dex_type_rejects_unknown() {
        let client = make_client();
        assert!(client.map_dex_type("CurveFi").is_err());
        assert!(client.map_dex_type("").is_err());
    }

    // ------------------------------------------------------------------------
    // format_route_for_log
    // ------------------------------------------------------------------------
    #[test]
    fn format_route_empty() {
        let client = make_client();
        assert_eq!(client.format_route_for_log(&[]), "Empty");
    }

    #[test]
    fn format_route_chains_tokens() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        let out = client.format_route_for_log(&steps);
        assert!(out.contains(format!("{:?}", a).as_str()));
        assert!(out.contains("→"));
    }

    // ------------------------------------------------------------------------
    // decode_revert_reason
    // ------------------------------------------------------------------------
    #[test]
    fn decode_revert_error_string_selector() {
        let client = make_client();
        // Fixture incompleto (só selector) — ainda classifica como Error(string)
        let msg = client.decode_revert_reason("execution reverted: data: 0x08c379a0");
        assert!(
            msg.contains("Error(") || msg.contains("08c379a0"),
            "msg={msg}"
        );
    }

    #[test]
    fn decode_revert_panic_selector() {
        let client = make_client();
        let msg = client.decode_revert_reason("execution reverted: data: 0x4e487b71");
        assert!(msg.contains("Panic") || msg.contains("4e487b71"), "msg={msg}");
    }

    #[test]
    fn decode_revert_unknown_selector() {
        let client = make_client();
        let msg = client.decode_revert_reason("execution reverted: data: 0xdeadbeef");
        assert!(
            msg.contains("CustomError") || msg.contains("deadbeef"),
            "msg={msg}"
        );
    }

    #[test]
    fn decode_revert_non_revert_passthrough() {
        let client = make_client();
        let msg = client.decode_revert_reason("some other rpc error");
        assert_eq!(msg, "some other rpc error");
    }

    #[test]
    fn paper_eth_call_uses_configured_paper_from() {
        std::env::remove_var(crate::core::paper_validation::ENV_PAPER_FROM);
        let mut cfg = Config::default();
        cfg.validation.paper_from = "0x152Aa7ecC490860115C4d1369a19C970f9e9eFFf".into();
        let wallet = Address::from_low_u64_be(0xDEAD);
        let from = crate::core::paper_validation::resolve_paper_from(&cfg, wallet);
        assert_eq!(
            format!("{:?}", from).to_ascii_lowercase(),
            "0x152aa7ecc490860115c4d1369a19c970f9e9efff"
        );
        assert_ne!(from, wallet);
    }

    #[test]
    fn paper_state_override_only_when_observation_active() {
        std::env::remove_var(crate::core::paper_validation::ENV_PAPER_VALIDATION);
        let mut cfg = Config::default();
        cfg.validation.paper_state_overrides = true;
        cfg.execution.dry_run = false;
        cfg.validation.paper_enabled = false;
        cfg.validation.dry_run_only = false;
        assert!(!crate::core::paper_validation::paper_state_overrides_enabled(&cfg));
        assert!(!crate::core::paper_validation::sends_forbidden(&cfg));

        cfg.validation.paper_enabled = true;
        assert!(crate::core::paper_validation::paper_state_overrides_enabled(&cfg));
        assert!(crate::core::paper_validation::sends_forbidden(&cfg));
    }

    // ------------------------------------------------------------------------
    // apply_complexity_filters — integração das duas validações
    // ------------------------------------------------------------------------
    #[test]
    fn complexity_filters_rejects_broken_route() {
        let client = make_client();
        let cfg = Config::default();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, c, a, U256::from(99)), // cadeia quebrada
        ];
        assert!(client.apply_complexity_filters(&steps, &cfg).is_err());
    }

    #[test]
    fn complexity_filters_rejects_zero_min_out() {
        let client = make_client();
        let cfg = Config::default();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::zero()),
            step(0, b, a, U256::from(99)),
        ];
        assert!(client.apply_complexity_filters(&steps, &cfg).is_err());
    }

    #[test]
    fn complexity_filters_ok_valid_cycle() {
        let client = make_client();
        let cfg = Config::default();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let steps = vec![
            step(0, a, b, U256::from(100)),
            step(0, b, a, U256::from(99)),
        ];
        assert!(client.apply_complexity_filters(&steps, &cfg).is_ok());
    }

    #[test]
    fn route_validation_enforces_hop_and_slippage_caps() {
        let client = make_client();
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        let c = Address::from_low_u64_be(3);
        let three_hops = vec![
            step(0, a, b, U256::from(100)),
            step(1, b, c, U256::from(99)),
            step(2, c, a, U256::from(98)),
        ];

        let mut cfg = Config::default();
        cfg.arbitrage.route_validation.max_hops = 2;
        assert!(client.apply_complexity_filters(&three_hops, &cfg).is_err());

        let two_hops = vec![
            step(0, a, b, U256::from(100)),
            step(1, b, a, U256::from(99)),
        ];
        cfg.arbitrage.route_validation.max_hops = 3;
        // Menos que 5 bps por hop não preserva piso de proteção.
        cfg.arbitrage.route_validation.max_cumulative_slippage = 0.09;
        cfg.execution.max_slippage_bps = 50;
        cfg.execution.hop_slippage_increase_bps = 0;
        assert!(client.apply_complexity_filters(&two_hops, &cfg).is_err());
    }

    // ------------------------------------------------------------------------
    // minimum_profit_raw — integer USD→token, fail-closed price
    // ------------------------------------------------------------------------
    fn sample_opp(price: Option<f64>, gas_usd: f64) -> ArbitrageOpportunity {
        let mut opp = ArbitrageOpportunity::default();
        opp.id = "t".into();
        opp.amount_in = U256::from(1_000_000u64);
        opp.gas_cost_usd = gas_usd;
        opp.token_price_usd = price;
        opp
    }

    #[test]
    fn minimum_profit_raw_stable_6dec() {
        let client = make_client();
        let mut cfg = Config::default();
        cfg.arbitrage.min_profit_absolute = "1.0".into();
        let opp = sample_opp(Some(1.0), 0.0);
        let raw = client.minimum_profit_raw(&opp, 6, &cfg).unwrap();
        assert_eq!(raw, U256::from(1_000_000u64));
    }

    #[test]
    fn minimum_profit_raw_rejects_missing_price() {
        let client = make_client();
        let cfg = Config::default();
        let opp = sample_opp(None, 0.1);
        let err = client.minimum_profit_raw(&opp, 6, &cfg).unwrap_err();
        assert!(
            err.to_string().contains("MissingProfitTokenPrice"),
            "err={err}"
        );
    }

    #[test]
    fn minimum_profit_raw_weth_price_2000() {
        let client = make_client();
        let mut cfg = Config::default();
        cfg.arbitrage.min_profit_absolute = "10.0".into();
        let opp = sample_opp(Some(2000.0), 0.0);
        let raw = client.minimum_profit_raw(&opp, 18, &cfg).unwrap();
        // $10 / $2000 = 0.005 ETH = 5e15 wei
        assert_eq!(raw, U256::from(5u64) * U256::exp10(15));
    }

    #[test]
    fn direct_fee_pct_is_zero_flashloan_nonzero() {
        // Document invariant at strategy layer: Direct maps fee to 0 bps.
        assert_eq!(fixed_usd::fee_pct_to_bps(0.0).unwrap(), 0);
        assert_eq!(fixed_usd::fee_pct_to_bps(0.0005).unwrap(), 5);
        assert_eq!(
            ArbitrageClient::gas_strategy_kind(&ExecutionStrategy::Direct),
            crate::core::gas::GasStrategyKind::Direct
        );
        assert_eq!(
            ArbitrageClient::gas_strategy_kind(&ExecutionStrategy::Flashloan),
            crate::core::gas::GasStrategyKind::WithFlashloan
        );
    }

    // ------------------------------------------------------------------------
    // V1 — CallbackData ABI encode round-trip (ethers decode = Solidity layout)
    // ------------------------------------------------------------------------
    #[test]
    fn callback_data_abi_roundtrip_preserves_fields() {
        use ethers::abi::{decode, ParamType, Token as AbiToken};

        let owner = Address::from_low_u64_be(0xA11);
        let other = Address::from_low_u64_be(0xB0B);
        let token_in = Address::from_low_u64_be(0xc1);
        let token_out = Address::from_low_u64_be(0xc2);
        let fee500 = ArbitrageClient::encode_v3_fee_extra_data(500);
        let step = AbiSwapStep {
            dex_type: 2, // UNISWAP_V3
            token_in,
            token_out,
            amount_out_min: U256::from(12_345u64),
            extra_data: fee500.clone(),
        };

        let cases: Vec<(Address, U256, &str)> = vec![
            (owner, U256::zero(), "owner_min0"),
            (other, U256::zero(), "other_recipient"),
            (owner, U256::from(2).pow(U256::from(130)), "min_gt_u128"),
        ];

        let step_tuple = ParamType::Tuple(vec![
            ParamType::Uint(8),
            ParamType::Address,
            ParamType::Address,
            ParamType::Uint(256),
            ParamType::Bytes,
        ]);
        let params = [
            ParamType::Address,
            ParamType::Array(Box::new(step_tuple)),
            ParamType::Uint(256),
        ];

        for (recipient, min_profit, name) in cases {
            let encoded = ArbitrageClient::encode_callback_data(recipient, &[step.clone()], min_profit);
            let decoded = decode(&params, encoded.as_ref()).expect(name);
            assert_eq!(decoded.len(), 3, "{name}");
            match &decoded[0] {
                AbiToken::Address(a) => assert_eq!(*a, recipient, "{name}"),
                o => panic!("{name}: expected Address, got {o:?}"),
            }
            match &decoded[2] {
                AbiToken::Uint(v) => assert_eq!(*v, min_profit, "{name}"),
                o => panic!("{name}: expected Uint, got {o:?}"),
            }
            let steps = match &decoded[1] {
                AbiToken::Array(a) => a,
                o => panic!("{name}: expected Array, got {o:?}"),
            };
            assert_eq!(steps.len(), 1, "{name}");
            let fields = match &steps[0] {
                AbiToken::Tuple(t) => t,
                o => panic!("{name}: expected Tuple, got {o:?}"),
            };
            assert_eq!(fields[0], AbiToken::Uint(U256::from(2u64)), "{name} dex");
            assert_eq!(fields[1], AbiToken::Address(token_in), "{name} in");
            assert_eq!(fields[2], AbiToken::Address(token_out), "{name} out");
            match &fields[4] {
                AbiToken::Bytes(b) => {
                    assert_eq!(b.as_slice(), fee500.as_ref(), "{name} extraData");
                    let fee = ArbitrageClient::decode_v3_fee_extra_data(&Bytes::from(b.clone()))
                        .expect(name);
                    assert_eq!(fee, 500, "{name} fee");
                }
                o => panic!("{name}: expected Bytes, got {o:?}"),
            }
        }
    }

    #[test]
    fn resolve_profit_recipient_prefers_wrapper_owner() {
        let client = make_client();
        let mut cfg = Config::default();
        cfg.wrapper.owner_address = Some("0x0000000000000000000000000000000000000A11".into());
        let got = client.resolve_profit_recipient(&cfg).unwrap();
        assert_eq!(got, Address::from_low_u64_be(0xA11));
    }

    #[test]
    fn resolve_profit_recipient_falls_back_to_wallet() {
        let client = make_client();
        let mut cfg = Config::default();
        cfg.wrapper.owner_address = None;
        let got = client.resolve_profit_recipient(&cfg).unwrap();
        assert_eq!(got, client.get_wallet_address().unwrap());
    }

    #[test]
    fn profit_recipient_mismatch_aborts_before_broadcast() {
        let owner = Address::from_low_u64_be(0xA11);
        let wallet = Address::from_low_u64_be(0xB0B);
        assert!(ArbitrageClient::ensure_profit_recipient_matches_owner(owner, owner).is_ok());
        let err = ArbitrageClient::ensure_profit_recipient_matches_owner(wallet, owner)
            .expect_err("wallet != owner must abort");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("ProfitRecipientMismatch"),
            "got: {msg}"
        );
        assert!(msg.contains("abort before broadcast"), "got: {msg}");
    }

    #[test]
    fn resolve_then_mismatch_when_cfg_owner_differs_from_onchain() {
        let client = make_client();
        let mut cfg = Config::default();
        // Config points at A11, but on-chain owner is wallet (simulated).
        cfg.wrapper.owner_address = Some("0x0000000000000000000000000000000000000A11".into());
        let resolved = client.resolve_profit_recipient(&cfg).unwrap();
        let onchain = client.get_wallet_address().unwrap();
        assert_ne!(resolved, onchain);
        assert!(ArbitrageClient::ensure_profit_recipient_matches_owner(resolved, onchain).is_err());
    }

    /// Export hex fixtures for Hardhat CallbackDataDecoder (ignored by default).
    #[test]
    #[ignore]
    fn export_callback_abi_fixtures() {
        use std::fs;
        use std::path::PathBuf;

        let owner = Address::from_low_u64_be(0xA11);
        let other = Address::from_low_u64_be(0xB0B);
        let step = AbiSwapStep {
            dex_type: 2,
            token_in: Address::from_low_u64_be(0xc1),
            token_out: Address::from_low_u64_be(0xc2),
            amount_out_min: U256::from(12_345u64),
            extra_data: ArbitrageClient::encode_v3_fee_extra_data(500),
        };
        let cases = vec![
            ("owner_recipient_fee500", owner, U256::zero()),
            ("other_recipient_min0", other, U256::zero()),
            (
                "high_minprofit_gt_u128",
                owner,
                U256::from(2).pow(U256::from(130)),
            ),
        ];
        let mut out = serde_json::json!({ "cases": [] });
        let arr = out["cases"].as_array_mut().unwrap();
        for (name, recipient, min_profit) in cases {
            let hex = format!(
                "0x{}",
                ethers::utils::hex::encode(
                    ArbitrageClient::encode_callback_data(recipient, &[step.clone()], min_profit)
                        .as_ref()
                )
            );
            arr.push(serde_json::json!({
                "name": name,
                "hex": hex,
                "expectRecipient": format!("{recipient:#x}"),
                "expectMinProfit": min_profit.to_string(),
                "expectFee": 500,
            }));
        }
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("test/fixtures");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("callback_abi_hex.json"), serde_json::to_string_pretty(&out).unwrap())
            .unwrap();
    }
}









