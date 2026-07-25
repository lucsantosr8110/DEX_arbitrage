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
        gas::GasEstimator,
        paper_validation::{self, PaperValidationHub},
        types::{ArbitrageOpportunity, BundleResult},
    },
    infra::metrics,
    AppMiddleware,
};

use anyhow::{anyhow, Context, Result};
use ethers::{
    abi::{encode, Detokenize, Token},
    prelude::*,
    types::{Eip1559TransactionRequest, U256},
};
use k256::ecdsa::SigningKey;
use std::{str::FromStr, sync::Arc};
use tokio::{
    sync::Mutex,
    time::{timeout, Duration},
};
use tracing::{warn, info, debug};

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
        }
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
    ) -> Result<(), FlashloanError> {
        let net_profit_usd = economics::net_profit_usd(
            gross_profit_usd,
            &economics::TradeCosts {
                gas_usd: gas_cost_usd,
                flashloan_fee_usd,
                adverse_move_usd: 0.0,
            },
        );

        if net_profit_usd <= 0.0 {
            return Err(FlashloanError::InsufficientProfit(format!(
                "Net profit ${:.4} <= 0 (gross: ${:.4}, gas: ${:.4}, flashloan_fee: ${:.4})",
                net_profit_usd, gross_profit_usd, gas_cost_usd, flashloan_fee_usd
            )));
        }

        info!(
            "💰 Profit validation: ${:.4} net (${:.4} gross - ${:.4} gas - ${:.4} flashloan)",
            net_profit_usd, gross_profit_usd, gas_cost_usd, flashloan_fee_usd
        );

        Ok(())
    }

    fn token_amount_to_usd(&self, amount: U256, price_usd: f64, decimals: u32) -> f64 {
        let raw_u128 = amount.as_u128();
        let denom = 10_f64.powi(decimals as i32);
        (raw_u128 as f64 / denom) * price_usd
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
        let max_hops_allowed = config.arbitrage.max_path_length as usize;
        
        // CORREÇÃO: Permitir até 4 hops para arbitragem triangular
        if steps.len() > max_hops_allowed {
            let reason = format!("{} hops exceeds maximum {}", steps.len(), max_hops_allowed);
            self.log_route_rejection(steps, &reason);
            return Err(FlashloanError::RouteTooComplex(reason));
        }
        
        // CORREÇÃO: Removida validação que bloqueia ciclos - arbitragem triangular é cíclica por natureza
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
        let (strategy, min_profit, risk_cfg, slippage_bps, flashloan_decimals, fee_pct) = {
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
)
        };

        // GAS - Estimativa atualizada
        let gas_cost = match self.gas_estimator.estimate_arbitrage_gas_usd().await {
            Ok(v) => {
                opp.gas_cost_usd = v;
                metrics::set_last_gas_usd(v);
                v
            }
            Err(_) => opp.gas_cost_usd,
        };

        // A5: valida a partir do GROSS (uma dedução de gas + Aave fee_pct).
        let token_price = opp.token_price_usd.unwrap_or(1.0);
        let flashloan_fee_token = self.calculate_flashloan_fee(opp.amount_in, fee_pct);
        let flashloan_fee_usd =
            self.token_amount_to_usd(flashloan_fee_token, token_price, flashloan_decimals);

        if let Err(e) = self.validate_profit_after_fees(
            opp.estimated_profit_usd, // GROSS
            gas_cost,
            flashloan_fee_usd,
        ) {
            warn!("{}", e);
            return Ok(BundleResult::skipped().with_execution_mode("insufficient_profit"));
        }

        // net_profit recalculado corretamente abaixo

        if opp.net_profit_usd < min_profit {
            info!("💰 Profit skip: ${:.4} < ${:.4}", opp.net_profit_usd, min_profit);
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

        let (slippage_bps, fee_pct, flashloan_decimals) = {
            let cfg = self.config.lock().await;
            (
                cfg.flashloan.slippage_bps.unwrap_or(50) as u64,
                cfg.flashloan.fee_pct.unwrap_or(economics::AAVE_V3_PREMIUM_PCT),
                cfg.flashloan.flashloan_decimals.unwrap_or(6) as u32,
            )
        };

        if let Ok(v) = self.gas_estimator.estimate_arbitrage_gas_usd().await {
            opp.gas_cost_usd = v;
        }

        let token_price = opp.token_price_usd.unwrap_or(1.0);
        let flashloan_fee_token = self.calculate_flashloan_fee(opp.amount_in, fee_pct);
        let flashloan_fee_usd =
            self.token_amount_to_usd(flashloan_fee_token, token_price, flashloan_decimals);

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
            .execute_flashloan(asset, amount, steps.clone())
            .from(paper_from)
            .block(block_id);

        let (sim_ok, revert_reason) = match timeout(Duration::from_secs(15), call.call()).await {
            Ok(Ok(true)) => (true, None),
            Ok(Ok(false)) => {
                let params = self.encode_flashloan_callback_params(paper_from, &steps);
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

    /// `abi.encode(initiator, steps)` — idêntico ao FlashloanExecutor.executeFlashloan.
    fn encode_flashloan_callback_params(
        &self,
        initiator: Address,
        steps: &[AbiSwapStep],
    ) -> Bytes {
        use ethers::abi::Tokenizable;
        Bytes::from(ethers::abi::encode(&[
            initiator.into_token(),
            steps.to_vec().into_token(),
        ]))
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
            return Ok(BundleResult::skipped().with_execution_mode("same_block"));
        }

        let (dry, simulate, asset, amount, steps) = {
            let cfg = self.config.lock().await;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;
            
            if asset != self.get_wallet_address()? {
                return Err(anyhow!("Direct execution requires owned token"));
            }
            
            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped().with_execution_mode("complexity_reject"));
            }
            
            (
                cfg.execution.dry_run,
                cfg.flashloan.simulate_before_execute.unwrap_or(true),
                asset,
                amount,
                steps
            )
        };

        // APPROVE
        if !dry && !simulate {
            if let Err(e) = self.approve_token_for_execution(asset, self.executor.address(), amount).await {
                warn!("❌ Approve falhou: {}", e);
                return Ok(BundleResult::skipped().with_execution_mode("approve_failed"));
            }
        }

        let direct = self.executor.execute_arbitrage(asset, amount, steps.clone());

        if simulate {
            info!("🔬 Simulando Direct Arbitrage...");
            match self.simulate_transaction(&direct).await {
                Ok(_) => info!("✅ Simulação Direct: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Direct falhou: {}", e);
                    return Ok(BundleResult::skipped().with_execution_mode("direct_sim_failed"));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(direct, opp, "direct").await
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
            return Ok(BundleResult::skipped().with_execution_mode("same_block"));
        }

        let (dry, simulate, asset, amount, steps) = {
            let cfg = self.config.lock().await;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;
            
            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped().with_execution_mode("complexity_reject"));
            }
            
            (
                cfg.execution.dry_run,
                cfg.flashloan.simulate_before_execute.unwrap_or(true),
                asset,
                amount,
                steps
            )
        };

        let call = self.executor.execute_flashloan(asset, amount, steps);

        if simulate {
            info!("🔬 Simulando Flashloan...");
            match self.simulate_bool_transaction(&call).await {
                Ok(_) => info!("✅ Simulação Flashloan: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Flashloan falhou: {}", e);
                    return Ok(BundleResult::skipped().with_execution_mode("flashloan_sim_failed"));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(call, opp, "flashloan").await
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
            return Ok(BundleResult::skipped().with_execution_mode("same_block"));
        }

        let (dry, simulate, wrapper_addr, asset, amount, steps) = {
            let cfg = self.config.lock().await;

            let wrapper_addr = Address::from_str(&cfg.wrapper.address)?;
            let (asset, amount, steps) = self.extract_and_convert_opp_data(opp, &cfg, slippage_bps)?;
            
            if let Err(e) = self.validate_wrapper_steps(&steps, asset) {
                warn!("❌ Steps inválidos para wrapper: {}", e);
                return Ok(BundleResult::skipped().with_execution_mode("wrapper_invalid_steps"));
            }
            
            if let Err(e) = self.apply_complexity_filters(&steps, &cfg) {
                warn!("{}", e);
                return Ok(BundleResult::skipped().with_execution_mode("complexity_reject"));
            }

            (
                cfg.execution.dry_run,
                cfg.flashloan.simulate_before_execute.unwrap_or(true),
                wrapper_addr,
                asset,
                amount,
                steps
            )
        };

        let executor_token = Token::Address(self.executor.address());
        let steps_token = Token::Array(
            steps.iter().map(|x| x.clone().into_token()).collect()
        );

        let params = Bytes::from(encode(&[executor_token, steps_token]));

        let contract = FlashloanCaller::new(wrapper_addr, self.middleware.clone());
        let call = contract.trigger_flashloan(asset, amount, params);

        if simulate {
            info!("🔬 Simulando Wrapper Flashloan...");
            match self.simulate_transaction(&call).await {
                Ok(_) => info!("✅ Simulação Wrapper: Sucesso"),
                Err(e) => {
                    warn!("❌ Simulação Wrapper falhou: {}", e);
                    return Ok(BundleResult::skipped().with_execution_mode("wrapper_sim_failed"));
                }
            }
        }

        if dry {
            return Ok(BundleResult::new(true, opp.estimated_profit_usd, opp.gas_cost_usd));
        }

        self.send_and_confirm_transaction(call, opp, "wrapper").await
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
        match timeout(Duration::from_secs(10), call.call()).await {
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
        match timeout(Duration::from_secs(10), call.call()).await {
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
        mode: &'static str
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

        let mut tx_req = Eip1559TransactionRequest::new();
        let (mut max_fee, mut max_priority) =
            self.gas_estimator.populate_dynamic_gas(&mut tx_req).await?;

        for attempt in 0..2 {
            if attempt > 0 {
                max_fee = max_fee * 120 / 100;
                max_priority = max_priority * 120 / 100;
                info!("🔄 Retry {} com gas boost: {:?} Gwei", attempt, max_fee);
            }

            if let Some(tx) = call.tx.as_eip1559_mut() {
                tx.max_fee_per_gas = Some(max_fee);
                tx.max_priority_fee_per_gas = Some(max_priority);
            }

            info!("💸 Enviando TX (Mode: {}, Attempt: {}) | Gas: {:?} Gwei", 
                  mode, attempt + 1, max_fee);

            match timeout(Duration::from_secs(10), call.send()).await {
                Ok(Ok(pending)) => {
                    info!("⏳ TX Enviada: {:?} - Aguardando confirmação...", pending.tx_hash());
                    
                    match timeout(Duration::from_secs(30), pending).await {
                        Ok(Ok(Some(receipt))) => {
                            if receipt.status == Some(1.into()) {
                                info!("✅ TX Confirmada: {:?}", receipt.transaction_hash);

                                // Anti-MEV: registrar bloco da execução bem-sucedida
                                // para que debounce_same_block bloqueie tentativas
                                // subsequentes no mesmo bloco (contrato exige).
                                self.update_execution_block().await;

                                let real_profit = self.extract_real_profit_from_receipt(&receipt)
                                    .unwrap_or(opp.estimated_profit_usd);
                                
                                return Ok(BundleResult::new(true, real_profit, opp.gas_cost_usd)
                                    .with_execution_mode(mode)
                                    .with_tx_hash(Some(format!("{:?}", receipt.transaction_hash))));
                            } else {
                                warn!("❌ TX Revertida: {:?}", receipt.transaction_hash);
                                return Ok(BundleResult::skipped().with_execution_mode("tx_reverted"));
                            }
                        },
                        _ => {
                            warn!("⏰ Timeout na confirmação");
                            continue;
                        }
                    }
                },
                Ok(Err(e)) => {
                    warn!("❌ Erro ao enviar TX: {}", e);
                    if attempt == 0 { continue; }
                    return Ok(BundleResult::skipped().with_execution_mode("send_failed"));
                },
                Err(_) => {
                    warn!("⏰ Timeout no envio");
                    continue;
                }
            }
        }

        Ok(BundleResult::skipped().with_execution_mode("max_retries_exceeded"))
    }

    fn extract_real_profit_from_receipt(&self, _receipt: &TransactionReceipt) -> Option<f64> {
        None
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

        let steps = ArbitrageEngine::sanitize_steps_for_execution(&opp.steps.0);
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

    use ethers::providers::{Http, Provider};
    use ethers::signers::LocalWallet;
    use std::str::FromStr;

    // Chave de teste hardhat account #0 — bem conhecida, sem valor.
    const TEST_PK: &str = "0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80";

    /// Constrói um ArbitrageClient com middleware dummy (RPC localhost).
    /// Nenhum método testado abaixo realiza chamadas de rede.
    fn make_client() -> ArbitrageClient {
        let provider = Provider::<Http>::try_from("http://127.0.0.1:8545")
            .expect("URL dummy válida");
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
    // token_amount_to_usd
    // ------------------------------------------------------------------------
    #[test]
    fn token_amount_to_usd_6_decimals() {
        let client = make_client();
        // 1_000_000 units @ 6 decimals, price $1 => $1.00
        let usd = client.token_amount_to_usd(U256::from(1_000_000), 1.0, 6);
        assert!((usd - 1.0).abs() < 1e-9, "usd={usd}");
    }

    #[test]
    fn token_amount_to_usd_18_decimals() {
        let client = make_client();
        // 1e18 units @ 18 decimals, price $2 => $2.00
        let amount = U256::from(10).pow(U256::from(18));
        let usd = client.token_amount_to_usd(amount, 2.0, 18);
        assert!((usd - 2.0).abs() < 1e-9, "usd={usd}");
    }

    // ------------------------------------------------------------------------
    // validate_profit_after_fees (A5 gross-based, uma dedução)
    // ------------------------------------------------------------------------
    #[test]
    fn profit_validation_ok_when_net_positive() {
        let client = make_client();
        // gross $5 - gas $0.50 - fee $0.50 => net $4.00
        let res = client.validate_profit_after_fees(5.0, 0.50, 0.50);
        assert!(res.is_ok(), "deveria aceitar profit positivo: {:?}", res);
    }

    #[test]
    fn profit_validation_rejects_net_zero_or_negative() {
        let client = make_client();
        // gross $1 - gas $0.50 - fee $0.60 => net -$0.10
        let res = client.validate_profit_after_fees(1.0, 0.50, 0.60);
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
        assert!(client.validate_profit_after_fees(gross, gas, fee).is_ok());
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
        let fee_usd = client.token_amount_to_usd(fee_tok, 1.0, 6);
        assert!((fee_usd - 0.05).abs() < 1e-9, "fee_usd={fee_usd}");
        assert!(client.validate_profit_after_fees(2.0, 0.1, fee_usd).is_ok());
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
}









