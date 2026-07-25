// ============================================================================
// src/core/flashloan.rs — FINAL v7.4 — CORREÇÕES CRÍTICAS APLICADAS
// ============================================================================

use ethers::abi::Tokenizable;
use crate::{
    config::Config,
    contracts::{FlashloanCaller, FlashloanExecutor, SwapStep as AbiSwapStep, ERC20},
    core::{
        gas::GasEstimator,
        types::{ArbitrageOpportunity, BundleResult},
        arbitrage::ArbitrageEngine,
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

        Self {
            executor,
            middleware,
            gas_estimator,
            config,
            execution_engine,
            last_exec_block: Arc::new(Mutex::new(None)),
        }
    }

    // ========================================================================
    // 🚨 VALIDAÇÕES CRÍTICAS - CORRIGIDAS
    // ========================================================================

    /// Calcula fee do flashloan (0.09%) 
    fn calculate_flashloan_fee(&self, amount: U256) -> U256 {
        amount * U256::from(9) / U256::from(10000)
    }

    /// Aplica slippage configurada
    fn apply_slippage(&self, amount: U256, slippage_bps: u64) -> U256 {
        let slippage_factor = U256::from(10000 - slippage_bps);
        amount * slippage_factor / U256::from(10000)
    }

    /// Valida se profit após fees é positivo
    fn validate_profit_after_fees(
    &self,
    estimated_profit_usd: f64,
    gas_cost_usd: f64,
    token_price_usd: f64,
    flashloan_amount: U256,
    flashloan_decimals: u32,
) -> Result<(), FlashloanError> {
        
        let flashloan_fee = self.calculate_flashloan_fee(flashloan_amount);
        let flashloan_fee_usd = self.token_amount_to_usd(
    flashloan_fee,
    token_price_usd,
    flashloan_decimals,
);
        
        let net_profit_usd = estimated_profit_usd - gas_cost_usd - flashloan_fee_usd;
        
        if net_profit_usd <= 0.0 {
            return Err(FlashloanError::InsufficientProfit(
                format!("Net profit ${:.4} <= 0 (profit: ${:.4}, gas: ${:.4}, flashloan_fee: ${:.4})", 
                       net_profit_usd, estimated_profit_usd, gas_cost_usd, flashloan_fee_usd)
            ));
        }
        
        info!("💰 Profit validation: ${:.4} net (${:.4} gross - ${:.4} gas - ${:.4} flashloan)", 
              net_profit_usd, estimated_profit_usd, gas_cost_usd, flashloan_fee_usd);
        
        Ok(())
    }

    fn token_amount_to_usd(&self, amount: U256, price_usd: f64, decimals: u32) -> f64 {
    let raw_u128 = amount.as_u128();
    let denom = 10_f64.powi(decimals as i32);
    (raw_u128 as f64 / denom) * price_usd
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
        let (strategy, min_profit, risk_cfg, slippage_bps, flashloan_decimals) = {
            let cfg = self.config.lock().await;

            (
    self.determine_execution_strategy(opp, &cfg).await,
    cfg.arbitrage.min_profit_absolute.parse::<f64>().unwrap_or(0.0001),
    cfg.risk.clone(),
    cfg.flashloan.slippage_bps.unwrap_or(50) as u64,
    cfg.flashloan.flashloan_decimals.unwrap_or(18) as u32,
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

        // Valida profit após fees — usa net_profit (já com gas/flashloan/slippage
        // deduzidos pelo arbitrage engine) para evitar dedução dupla.
        if let Err(e) = self.validate_profit_after_fees(
    opp.net_profit_usd,
    gas_cost,
    opp.token_price_usd.unwrap_or(0.0),
    opp.amount_in,
    flashloan_decimals,
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
        if err.contains("execution reverted") {
            if let Some(data) = err.split("data: 0x").nth(1) {
                match data.get(0..8) {
                    Some("08c379a0") => return "Revert: Error(string)".to_string(),
                    Some("4e487b71") => return "Revert: Panic(uint256)".to_string(),
                    _ => return format!("Revert: {}", &data[0..8]),
                }
            }
        }
        err.to_string()
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
        slippage_bps: u64
    ) -> Result<(Address, U256, Vec<AbiSwapStep>)> {

        let steps = ArbitrageEngine::sanitize_steps_for_execution(&opp.steps.0);
        let first = steps.first().context("Steps vazios")?;

        let token_in = self.get_token_addr(&first.token_in, cfg)?;
        let amount_in = opp.amount_in;

        let mut abi_steps = vec![];

        for s in steps {
            let amount_out_min = self.apply_slippage(s.amount_out_min, slippage_bps);
            
            abi_steps.push(AbiSwapStep {
                dex_type: self.map_dex_type(&s.dex_name)?,
                token_in: self.get_token_addr(&s.token_in, cfg)?,
                token_out: self.get_token_addr(&s.token_out, cfg)?,
                amount_out_min,
                extra_data: Bytes::new(),
            });
        }

        info!("🔄 Rota convertida: {} steps | Amount: {} | Slippage: {} bps", 
              abi_steps.len(), amount_in, slippage_bps);

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









