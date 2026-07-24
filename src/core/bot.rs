// ============================================================
// src/core/bot.rs — v4.8.4-MEV-FIXED - CLEAN
// ============================================================

use crate::{
    config::Config,
    core::{
        arbitrage::ArbitrageEngine,
        flashloan::ArbitrageClient,
        gas::GasEstimator,
        risk::RiskManager,
        types::{ArbitrageOpportunity, BundleResult},
    },
    infra::metrics,
    utils::telegram::TelegramNotifier,
    AppMiddleware,
};

use anyhow::Result;
use chrono::Utc;
use ethers::types::Address;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    sync::Arc,
};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, info};

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
            .unwrap_or_else(|| {
                "0xb391aEebB4Db4e99A456B28d29d3AF50193F078F".into()
            });

        let executor_address: Address =
            executor_address_str.parse().unwrap_or_default();

        // ✅ ArbitrageClient sem execution_engine
        let arbitrage_client = ArbitrageClient::new(
            executor_address,
            client.clone(),
            config.clone(),
            None,
        );

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
        if self.telegram.is_enabled() {
            let _ = self.telegram.notify_opportunity(
                opportunity.spread_percent,
                opportunity.estimated_profit_usd,
                &opportunity.pair,
                &[&opportunity.buy_dex, &opportunity.sell_dex],
            ).await;
        }

        self.arbitrage_client
            .execute_opportunity(&mut opportunity)
            .await
    }

    // ============================================================
    // 🧾 Processamento e auditoria de preços
    // ============================================================
    pub async fn process_prices(
        &mut self,
        prices: HashMap<String, HashMap<String, f64>>,
    ) -> Result<(), anyhow::Error> {
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

        let opportunities = self
            .arbitrage_engine
            .find_arbitrage_opportunities(&prices, &cfg_snapshot)
            .await;

        if opportunities.is_empty() {
            info!("📭 Ciclo sem oportunidades acima do threshold");
        } else {
            let best = &opportunities[0];
            let all_pairs: Vec<String> = opportunities
                .iter()
                .map(|o| format!("{}@{:.2}%", o.pair, o.spread_percent))
                .collect();
            info!(
                "🎯 {} oportunidades | melhor: {} spread={:.4}% net=${:.6} | [{}]",
                opportunities.len(),
                best.pair,
                best.spread_percent,
                best.net_profit_usd,
                all_pairs.join(", ")
            );
            let _ = self.handle_opportunity(opportunities.into_iter().next().unwrap()).await;
        }

        Ok(())
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
