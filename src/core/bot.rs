// ============================================================
// src/core/bot.rs — v4.8.4-MEV-FIXED - CLEAN
// ============================================================

use crate::{
    config::Config,
    core::{
        arbitrage::ArbitrageEngine,
        flashloan::ArbitrageClient,
        gas::GasEstimator,
        paper_validation,
        risk::RiskManager,
        types::{ArbitrageOpportunity, BundleResult, ExecutionOutcome},
    },
    infra::metrics,
    utils::telegram::TelegramNotifier,
    AppMiddleware,
};

use anyhow::Result;
use chrono::Utc;
use ethers::types::Address;
use std::{collections::HashMap, fs::OpenOptions, io::Write, sync::Arc};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info, warn};

/// ============================================================
/// 🤖 Estrutura principal do Bot
/// ============================================================
pub struct Bot {
    pub config: Arc<Mutex<Config>>,
    pub arbitrage_engine: ArbitrageEngine,
    pub risk_manager: RiskManager,
    pub arbitrage_client: ArbitrageClient,
    pub execution_mode: ExecutionMode,
    pub telegram: Arc<TelegramNotifier>,
}

#[derive(Debug, Clone)]
pub enum ExecutionMode {
    Direct,
    Flashloan,
    Hybrid,
}

impl Bot {
    // ============================================================
    // 🔧 Inicialização principal (sem MEV)
    // ============================================================
    pub async fn new(
        client: Arc<AppMiddleware>,
        config: Arc<Mutex<Config>>,
        telegram: Arc<TelegramNotifier>,
    ) -> Self {
        Self::new_with_engine(client, config, telegram, None).await
    }

    // ============================================================
    // 🔍 Acesso controlado ao ArbitrageEngine (READ-ONLY)
    // ⚠️ Deve ser usado apenas enquanto o MutexGuard do Bot existir
    // ============================================================
    pub fn get_arbitrage_engine(&self) -> &ArbitrageEngine {
        &self.arbitrage_engine
    }

    // ============================================================
    // 🔧 Inicialização principal (compatibilidade preservada)
    // ============================================================
    pub async fn new_with_engine(
        client: Arc<AppMiddleware>,
        config: Arc<Mutex<Config>>,
        telegram: Arc<TelegramNotifier>,
        _execution_engine: Option<()>, // Placeholder
    ) -> Self {
        let _gas_estimator = Arc::new(GasEstimator::new(client.clone(), config.clone()));

        // ✅ Usa construtor padrão do ArbitrageEngine (sem price_feed)
        let arbitrage_engine = ArbitrageEngine::new(client.clone());

        let cfg_guard = config.lock().await;
        let risk_manager = RiskManager::new(cfg_guard.risk.clone());

        let executor_address_str = cfg_guard
            .flashloan
            .executor_address
            .clone()
            .unwrap_or_else(|| "0xb391aEebB4Db4e99A456B28d29d3AF50193F078F".into());

        let executor_address: Address = executor_address_str.parse().unwrap_or_default();

        // ✅ ArbitrageClient sem execution_engine
        let arbitrage_client =
            ArbitrageClient::new(executor_address, client.clone(), config.clone(), None);
        // B2: carrega oráculo EWMA de gas do path configurado (se houver).
        arbitrage_client.init_gas_oracle().await;
        // B7: ajusta threshold do circuit breaker de perda via config.
        arbitrage_client.init_profit_ledger().await;

        let execution_mode = if cfg_guard.flashloan.enabled {
            if cfg_guard
                .arbitrage
                .default_trade_amount
                .parse::<f64>()
                .unwrap_or(0.0)
                > 0.0
            {
                ExecutionMode::Hybrid
            } else {
                ExecutionMode::Flashloan
            }
        } else {
            ExecutionMode::Direct
        };

        drop(cfg_guard);

        // M5: sincroniza prêmio Aave antes da primeira descoberta/ranking.
        arbitrage_client.refresh_flashloan_premium().await;

        metrics::inc_bot_start_total();

        if telegram.is_enabled() {
            let _ = telegram
                .send_alert(
                    "Bot Inicializado",
                    "Bot core inicializado com sucesso (versão básica)",
                )
                .await;
        }

        Self {
            config,
            arbitrage_engine,
            risk_manager,
            arbitrage_client,
            execution_mode,
            telegram,
        }
    }

    // ============================================================
    // 🧭 Inicialização padrão (com hot reload e log)
    // ============================================================
    pub async fn init(
        client: Arc<AppMiddleware>,
        config: Arc<Mutex<Config>>,
        telegram: Arc<TelegramNotifier>,
    ) -> Result<Self> {
        info!("⏳ Inicializando bot com suporte a Flashloan...");
        let cfg = config.lock().await;
        info!("📄 Config carregada (versão {:?})", cfg.general.version);
        drop(cfg);
        Ok(Self::new_with_engine(client, config, telegram, None).await)
    }

    // ============================================================
    // 🧭 Inicialização com ExecutionEngine (placeholder)
    // ============================================================
    pub async fn init_with_engine(
        client: Arc<AppMiddleware>,
        config: Arc<Mutex<Config>>,
        telegram: Arc<TelegramNotifier>,
        _execution_engine: Option<()>,
    ) -> Result<Self> {
        info!("⏳ Inicializando bot (modo compatibilidade)...");
        let cfg = config.lock().await;
        info!("📄 Config carregada (versão {:?})", cfg.general.version);
        drop(cfg);
        Ok(Self::new_with_engine(client, config, telegram, None).await)
    }

    // ============================================================
    // 🎯 Execução de oportunidade (com Telegram)
    // ============================================================
    //
    // A escolha de estratégia — incluindo o gate de `dry_run` — acontece num único
    // lugar: `ArbitrageClient::determine_execution_strategy` (core/flashloan.rs).
    // Existia aqui uma segunda cópia que *logava* `dry_run` mas devolvia
    // `WrapperFlashloan` mesmo assim; era inofensiva só porque a de `flashloan.rs`
    // decidia por último. O efeito visível era um log mentindo ("🚀 Executando via
    // WrapperFlashloan" seguido de nada) e o risco de que qualquer refactor que
    // invertesse a ordem virasse envio real de transação em dry run.
    pub async fn handle_opportunity(
        &mut self,
        mut opportunity: ArbitrageOpportunity,
    ) -> Result<BundleResult> {
        // C2: rota não-suportada pelo FlashloanExecutor (ex.: Curve) é descartada
        // antes de qualquer estimativa de gas/RPC — espelha o gate do paper path
        // (bot.rs:276). Finder ainda emite opps Curve p/ replay cross-model
        // (paper), mas exec real nunca as processa.
        if !paper_validation::route_executor_supported(&opportunity) {
            info!(
                "🚫 Skip exec: rota não-suportada pelo executor ({}): {}",
                opportunity.buy_dex, opportunity.pair
            );
            return Ok(BundleResult::skipped().with_execution_mode("route_unsupported"));
        }

        if self.telegram.is_enabled() {
            let _ = self
                .telegram
                .notify_opportunity(
                    opportunity.spread_percent,
                    opportunity.estimated_profit_usd,
                    &opportunity.pair,
                    &[&opportunity.buy_dex, &opportunity.sell_dex],
                )
                .await;
        }

        self.arbitrage_client
            .execute_opportunity(&mut opportunity)
            .await
    }

    // ============================================================
    // 🧾 Processamento e auditoria de preços
    // ============================================================
    //
    // FASE 7: a detecção (descoberta + seleção) roda sob o lock do bot; o envio/
    // confirmação de tx NÃO. `select_opportunities` devolve as top-N opps sem
    // executá-las; `main.rs` executa fora do lock (clonando ArbitrageClient, que
    // é Clone, + telegram Arc). Isso evita segurar MutexGuard através de awaits
    // longos de broadcast/confirmação, que travava TUI/preços por 30-60s.
    pub async fn process_prices(
        &mut self,
        prices: HashMap<String, HashMap<String, f64>>,
    ) -> Result<(), anyhow::Error> {
        let selected = self.select_opportunities(prices).await?;
        for opp in selected {
            let _ = self.handle_opportunity(opp).await;
        }
        Ok(())
    }

    /// Descoberta + seleção top-N sob lock. Sem envio de tx. Paper path observa
    /// inline e devolve `Vec::new()`. Non-paper devolve até `top_n_opportunities`
    /// opps ordenadas por net profit descrescente.
    pub async fn select_opportunities(
        &mut self,
        prices: HashMap<String, HashMap<String, f64>>,
    ) -> Result<Vec<ArbitrageOpportunity>, anyhow::Error> {
        debug!("🧠 Processando {} conjuntos de preços", prices.len());

        if std::env::var("BOT_AUDIT_PRICES")
            .unwrap_or_else(|_| "false".into())
            .to_lowercase()
            == "true"
        {
            let timestamp = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();
            let csv_path = "logs/prices_audit.csv";

            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(csv_path)?;

            for (dex, pairs) in &prices {
                for (pair, price) in pairs {
                    writeln!(file, "{},{},{},{}", timestamp, dex, pair, price)?;
                }
            }
        }

        let cfg_snapshot = { self.config.lock().await.clone() };
        self.risk_manager.config = cfg_snapshot.risk.clone();

        let observation = paper_validation::observation_active(&cfg_snapshot);
        let exec_min = paper_validation::exec_min_spread_pct(&cfg_snapshot);

        // Descoberta: em paper usa observe_min_spread; fora paper = só exec min.
        let discovery_cfg = if observation {
            let observe_min = paper_validation::resolve_observe_min_spread(&cfg_snapshot);
            let mut obs = cfg_snapshot.clone();
            obs.arbitrage.min_spread_percent = format!("{}", observe_min);
            // Micro-edges: permite passar filtro de lucro na conversão USDT
            // (execução real continua gateada por exec min_spread + min_profit).
            obs.arbitrage.min_profit_threshold_usd = Some(-1.0e9);
            obs
        } else {
            cfg_snapshot.clone()
        };

        let mut opportunities = self
            .arbitrage_engine
            .find_arbitrage_opportunities(&prices, &discovery_cfg)
            .await;

        if observation {
            let max_n = cfg_snapshot.validation.observe_max_per_cycle.max(1) as usize;
            if opportunities.is_empty() {
                info!(
                    "📭 PAPER observe: nenhuma opp >= observe_min_spread ({:.4}%)",
                    paper_validation::resolve_observe_min_spread(&cfg_snapshot)
                );
            } else {
                info!(
                    "📄 PAPER observe: {} opps (exec_min={:.4}% observe={:.4}%) — envio OFF",
                    opportunities.len(),
                    exec_min,
                    paper_validation::resolve_observe_min_spread(&cfg_snapshot)
                );
                let mut observed = 0usize;
                let mut skipped_unsupported = 0usize;
                for mut opp in opportunities {
                    if !paper_validation::route_executor_supported(&opp) {
                        skipped_unsupported += 1;
                        continue;
                    }
                    let would = paper_validation::would_execute(opp.spread_percent, &cfg_snapshot);
                    if let Err(e) = self
                        .arbitrage_client
                        .paper_observe_opportunity(&mut opp, would)
                        .await
                    {
                        warn!(
                            target: "paper_validation",
                            "paper observe failed pair={} would_execute={}: {:#}",
                            opp.pair, would, e
                        );
                    }
                    observed += 1;
                    if observed >= max_n {
                        break;
                    }
                }
                if observed == 0 {
                    info!(
                        "📭 PAPER observe: 0 rotas executor-supported (skipped_unsupported={})",
                        skipped_unsupported
                    );
                }
            }
            return Ok(Vec::new());
        }

        if opportunities.is_empty() {
            info!("📭 Ciclo sem oportunidades acima do threshold");
            return Ok(Vec::new());
        }

        // Top-N configurável, ordenado por net profit descrescente. main.rs
        // executa fora do lock; falhas pré-broadcast (AbortedPreBroadcast) tentam
        // a próxima; qualquer outcome de broadcast/terminal encerra o ciclo.
        let top_n = cfg_snapshot.execution.top_n_opportunities.max(1) as usize;
        opportunities.sort_by(|a, b| {
            b.net_profit_usd
                .partial_cmp(&a.net_profit_usd)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        let best = &opportunities[0];
        let all_pairs: Vec<String> = opportunities
            .iter()
            .take(top_n)
            .map(|o| format!("{}@{:.2}%", o.pair, o.spread_percent))
            .collect();
        info!(
            "🎯 {} oportunidades | melhor: {} spread={:.4}% net=${:.6} | top{}=[{}]",
            opportunities.len(),
            best.pair,
            best.spread_percent,
            best.net_profit_usd,
            top_n,
            all_pairs.join(", ")
        );
        Ok(opportunities.into_iter().take(top_n).collect())
    }

    // ============================================================
    // 🔁 Loop principal (com Telegram)
    // ============================================================
    pub async fn run(
        &mut self,
        mut price_rx: mpsc::Receiver<HashMap<String, HashMap<String, f64>>>,
        mut shutdown_rx: broadcast::Receiver<()>,
    ) -> Result<(), anyhow::Error> {
        info!("🤖 Bot iniciado — modo {:?}", self.execution_mode);
        metrics::set_bot_status(1);

        loop {
            tokio::select! {
                _ = shutdown_rx.recv() => break,
                Some(prices) = price_rx.recv() => {
                    let _ = self.process_prices(prices).await;
                }
            }
        }

        metrics::set_bot_status(0);
        Ok(())
    }

    // ============================================================
    // 🔧 Acesso ao Telegram
    // ============================================================
    pub fn telegram(&self) -> &Arc<TelegramNotifier> {
        &self.telegram
    }
}

// =====================================================================
// FASE 7: execução fora do lock do bot
// =====================================================================
//
// `select_opportunities` roda sob lock (descoberta). A execução (envio/
// confirmação de tx) roda aqui, sem segurar o MutexGuard do bot. O
// `ArbitrageClient` é Clone e `TelegramNotifier` é Arc — clonados sob lock
// e usados fora dele.

/// Executa uma oportunidade sem segurar o lock do bot. Contém o mesmo gate
/// de rota suportada + notificação telegram de `handle_opportunity`, mas
/// opera sobre um `ArbitrageClient` clonado.
pub async fn execute_opportunity_standalone(
    client: &ArbitrageClient,
    telegram: &TelegramNotifier,
    mut opportunity: ArbitrageOpportunity,
) -> Result<BundleResult> {
    if !paper_validation::route_executor_supported(&opportunity) {
        info!(
            "🚫 Skip exec: rota não-suportada pelo executor ({}): {}",
            opportunity.buy_dex, opportunity.pair
        );
        return Ok(BundleResult::skipped()
            .with_execution_mode("route_unsupported")
            .with_outcome(ExecutionOutcome::AbortedPreBroadcast {
                reason: "route unsupported by executor".into(),
            }));
    }

    if telegram.is_enabled() {
        let _ = telegram
            .notify_opportunity(
                opportunity.spread_percent,
                opportunity.estimated_profit_usd,
                &opportunity.pair,
                &[&opportunity.buy_dex, &opportunity.sell_dex],
            )
            .await;
    }

    client.execute_opportunity(&mut opportunity).await
}

/// Decide se o ciclo deve tentar a próxima oportunidade do top-N.
///
/// Tenta a próxima **só** em abort pré-broadcast (simulação/complexidade/
/// approve/rota não-suportada/falta de preço). Qualquer outcome de broadcast
/// (Confirmed, Reverted, TimeoutStuck, Dropped) ou SameBlockRejected encerra
/// o ciclo — não há concorrência segura de nonce/liquidez para paralelo.
pub fn should_try_next_opp(res: &BundleResult) -> bool {
    matches!(
        res.outcome,
        Some(ExecutionOutcome::AbortedPreBroadcast { .. })
    )
}

#[cfg(test)]
mod should_try_next_tests {
    use super::*;
    use ethers::types::U256;

    #[test]
    fn continues_only_on_aborted_pre_broadcast() {
        let aborted = BundleResult::skipped().with_outcome(ExecutionOutcome::AbortedPreBroadcast {
            reason: "sim failed".into(),
        });
        assert!(should_try_next_opp(&aborted));

        let h = ethers::types::H256::repeat_byte(0x1);
        let reverted = BundleResult::skipped().with_outcome(ExecutionOutcome::Reverted {
            tx_hash: h,
            reason: None,
            gas_used: None,
        });
        assert!(
            !should_try_next_opp(&reverted),
            "revert é terminal — não tenta próxima"
        );

        let stuck = BundleResult::skipped().with_outcome(ExecutionOutcome::TimeoutStuck {
            nonce: U256::from(1),
            latest_tx_hash: Some(h),
        });
        assert!(!should_try_next_opp(&stuck), "timeout stuck é terminal");

        let same_block = BundleResult::skipped()
            .with_outcome(ExecutionOutcome::SameBlockRejected { tx_hash: None });
        assert!(
            !should_try_next_opp(&same_block),
            "same-block não tenta próxima"
        );

        let no_outcome = BundleResult::skipped();
        assert!(
            !should_try_next_opp(&no_outcome),
            "sem outcome = não continua"
        );
    }
}
