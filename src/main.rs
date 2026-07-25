// ============================================================
// src/main.rs — v4.8.4-HYBRID-SAFE (CORRIGIDO TYPE ERROR)
// ============================================================

use anyhow::{Context, Result};
use ethers::{
    providers::{Middleware, Provider, Ws},
    types::H160,
};
use futures::future;
use std::{collections::HashMap, path::PathBuf, str::FromStr, sync::Arc, time::Duration};
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::{
    filter::LevelFilter,
    fmt::{self, writer::MakeWriterExt},
    prelude::*,
};

use flashloan_bot::{
    config::Config,
    core::bot::Bot,
    dex::{
        circuit_breaker::DexCircuitBreaker, manager::DexManager, price_cache::PriceCache,
        radar::start_high_hit_rate_radar,
    },
    execution::{bundle_sender::MevConfig, gwei, ExecutionEngine},
    infra::{
        metrics,
        rpc_provider::{is_usable_endpoint, RpcProvider},
    },
    tui,
    utils::telegram::TelegramNotifier,
};

// ============================================================
// 0️⃣ FUNÇÃO AUXILIAR PARA LOG DE CONFIGURAÇÃO
// ============================================================
fn log_config_snapshot(config: &Config) {
    info!("═══════════════════════════════════════════════════════════════════");
    info!("⚙️  CONFIGURAÇÃO DO BOT");
    info!("═══════════════════════════════════════════════════════════════════");
    info!(
        "  Versão: {}",
        config.general.version.as_deref().unwrap_or("unknown")
    );
    info!("  Modo: {} | Dry Run: {}", if config.flashloan.enabled { "FLASHLOAN" } else { "DIRECT" }, config.execution.dry_run);
    info!("  Gas: priority={:.1} gwei, max={} gwei", config.gas.priority_gwei, config.gas.max_gwei);
    info!(
        "  Min Profit: ${:.4} | Min Spread: {}%",
        config
            .arbitrage
            .min_profit_absolute
            .parse::<f64>()
            .unwrap_or(0.0),
        config.arbitrage.min_spread_percent
    );
    info!("  Capital: ${}", config.flashloan.capital_usd);
    info!("═══════════════════════════════════════════════════════════════════");
}

// ============================================================
// MAIN
// ============================================================
#[tokio::main]
async fn main() -> Result<()> {
    // ============================================================
    // 1️⃣ Logging e Inicialização
    // ============================================================
    let use_json_logs = std::env::var("BOT_JSON_LOGS")
        .unwrap_or_else(|_| "false".into())
        .to_lowercase()
        == "true";

    let env_filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into());

    if use_json_logs {
        tracing_subscriber::fmt()
            .json()
            .with_current_span(false)
            .with_target(false)
            .init();
        info!("🧾 Logging JSON habilitado (UTC).");
    } else {
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .parse(env_filter)
            .context("❌ Falha ao parsear RUST_LOG")?;

        let stdout = std::io::stdout.with_max_level(Level::INFO);

        let fmt_layer = fmt::layer()
            .compact()
            .with_ansi(true)
            .with_target(false)
            .with_writer(stdout);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();

        info!("🧾 Logging padrão habilitado (UTC, com cores).");
    }

    info!("🚀 Iniciando Flashloan DEX Arbitrage Bot v4.8.4-HYBRID-SAFE...");

    // ============================================================
    // 2️⃣ Carregamento de Configuração e Variáveis (.env)
    // ============================================================
    let env_path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".env");
    if dotenvy::from_path(&env_path).is_err() {
        if dotenvy::dotenv().is_err() {
            warn!("⚠️ Não foi possível carregar o arquivo .env.");
        } else {
            info!("✅ Variáveis de ambiente carregadas do .env padrão.");
        }
    } else {
        info!("✅ Variáveis de ambiente carregadas do arquivo .env no diretório raiz do Cargo.");
    }

    let config_path: PathBuf = {
        let from_env = std::env::var("CONFIG_FILE").ok();
        let p = from_env.unwrap_or_else(|| "config/config.toml".to_string());
        PathBuf::from(p.replace('\\', "/"))
    };
    info!("🧩 Usando arquivo de configuração: {}", config_path.display());

    let config = Arc::new(Mutex::new(
        Config::from_file(config_path.clone()).with_context(|| {
            format!(
                "❌ Falha ao ler o arquivo de configuração: {}",
                config_path.display()
            )
        })?,
    ));
    let cfg_unlocked = {
        let lock = config.lock().await;
        Arc::new(lock.clone())
    };

    // ============================================================
    // 3️⃣ Telegram Notifier
    // ============================================================
    let telegram = match TelegramNotifier::init_from_config(&cfg_unlocked).await {
        Ok(tg) => Arc::new(tg),
        Err(e) => {
            warn!("⚠️ TelegramNotifier falhou: {}", e);
            Arc::new(TelegramNotifier::disabled())
        }
    };

    log_config_snapshot(&cfg_unlocked);

    // ============================================================
    // 4️⃣ RPC Providers (HTTP e WS)
    // ============================================================
    // Fonte única de endpoints: o `.env` sobrepõe o TOML quando presente, senão o
    // bloco [network] do config manda. Antes o `.env` era obrigatório e o TOML era
    // silenciosamente ignorado — duas fontes de verdade para a mesma coisa.
    let rpc_endpoints: Vec<String> = match std::env::var("BOT_RPC_ENDPOINTS") {
        Ok(raw) if !raw.trim().is_empty() => {
            let list: Vec<String> = raw
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
            info!(
                "🌐 Endpoints RPC vindos de BOT_RPC_ENDPOINTS ({} entradas).",
                list.len()
            );
            list
        }
        _ => {
            let list = cfg_unlocked
                .network
                .rpc_endpoints
                .clone()
                .unwrap_or_default();
            info!(
                "🌐 BOT_RPC_ENDPOINTS ausente — usando [network].rpc_endpoints do config ({} entradas).",
                list.len()
            );
            list
        }
    };

    if !rpc_endpoints.iter().any(|u| is_usable_endpoint(u)) {
        anyhow::bail!(
            "❌ Nenhum endpoint RPC utilizável. Defina BOT_RPC_ENDPOINTS no .env ou \
             [network].rpc_endpoints no config (placeholders ${{VAR}} não resolvidos não contam)."
        );
    }

    let private_key = std::env::var("PRIVATE_KEY").context("❌ PRIVATE_KEY ausente no .env")?;

    let client_http = RpcProvider::connect_http_with_fallback(
        &cfg_unlocked.network,
        &private_key,
        &rpc_endpoints,
    )
    .await
    .context("❌ Falha ao conectar via HTTP (fallback esgotado)")?;

    let chain_id = client_http.get_chainid().await?.as_u64();
    info!("🌐 RPC HTTP conectado (chain_id = {}).", chain_id);

    info!("📡 Conectando WebSocket...");
    let client_ws: Arc<Provider<Ws>> = RpcProvider::connect_ws(&cfg_unlocked.network)
        .await
        .context("❌ Falha ao conectar via WebSocket")?;
    info!("✅ WebSocket conectado.");

    // ============================================================
    // 5️⃣ DexManager e Cache de Preços
    // ============================================================
    let price_cache = Arc::new(PriceCache::new(Duration::from_secs(10)));
    let dex_manager = Arc::new(
        DexManager::new(
            client_http.clone(),
            cfg_unlocked.clone(),
            price_cache.clone(),
        )
        .await
        .context("❌ Falha ao inicializar DexManager")?,
    );
    info!("🧩 DexManager inicializado com sucesso.");

    // ============================================================
    // 6️⃣ Inicialização do ExecutionEngine (v4.8.4-HYBRID-SAFE)
    // ============================================================
    let mev_cfg = MevConfig {
        enabled: cfg_unlocked.mev.enabled,
        relay_url: cfg_unlocked.mev.relay_url.clone(),
        signer_address: cfg_unlocked.mev.signer_address.clone(),
        min_tip_matic: cfg_unlocked.mev.min_tip_matic.clone(),
        target_block_offset: cfg_unlocked.mev.target_block_offset,
        timeout_seconds: cfg_unlocked.mev.timeout_seconds,
    };

    let executor_address = cfg_unlocked
        .flashloan
        .executor_address
        .as_ref()
        .and_then(|addr| H160::from_str(addr).ok())
        .ok_or_else(|| anyhow::anyhow!("Executor address inválido ou ausente"))?;

    let _execution_engine = match ExecutionEngine::new(
        client_http.clone(),
        Arc::new(client_http.signer().clone()),
        mev_cfg,
        1,              // cooldown_seconds
        executor_address,
        Some(gwei(25)), // piso do priority fee (PIP-35)
    )
    .await
    {
        Ok(engine) => {
            info!("✅ ExecutionEngine (v4.8.4-HYBRID-SAFE) inicializado com sucesso.");
            Some(engine)
        }
        Err(e) => {
            warn!("⚠️ Falha ao inicializar ExecutionEngine: {:?}", e);
            None
        }
    };

    // ============================================================
    // 7️⃣ Circuit Breaker
    // ============================================================
    let circuit_breaker = Arc::new(DexCircuitBreaker::new(5, 30));

    // ============================================================
    // 8️⃣ Inicialização do Bot
    // ============================================================
    let bot = match Bot::init_with_engine(
        client_http.clone(),
        config.clone(),
        telegram.clone(),
        None,
    )
    .await
    {
        Ok(bot) => bot,
        Err(e) => {
            warn!("⚠️ Bot::init_with_engine() falhou: {:?}", e);
            Bot::new_with_engine(
                client_http.clone(),
                config.clone(),
                telegram.clone(),
                None,
            )
            .await
        }
    };

    let bot = Arc::new(Mutex::new(bot));

    // ============================================================
    // 9️⃣ Canais e Radar
    // ============================================================
    let (price_tx, price_rx) = mpsc::channel::<HashMap<String, HashMap<String, f64>>>(256);
    let price_rx = Arc::new(Mutex::new(price_rx));
    let (shutdown_tx, _) = broadcast::channel::<()>(4);

    let radar_task = {
        let client_ws = client_ws.clone();
        let dex_manager = dex_manager.clone();
        let config = config.clone();
        let price_tx = price_tx.clone();
        let price_cache = price_cache.clone();
        let circuit_breaker = circuit_breaker.clone();
        let sd_rx = shutdown_tx.subscribe();
        let telegram = telegram.clone();

        tokio::spawn(async move {
            info!("📡 High Hit Rate Radar iniciado.");
            if let Err(e) = start_high_hit_rate_radar(
                client_ws,
                dex_manager,
                config,
                price_cache,
                circuit_breaker,
                price_tx,
                sd_rx,
            )
            .await
            {
                error!("❌ Radar erro: {:?}", e);
                let _ = telegram
                    .send_error_alert("Radar", &format!("{:?}", e))
                    .await;
            }
        })
    };

    // ============================================================
    // 🔟 Executor Principal
    // ============================================================
    let bot_task = {
        let bot = bot.clone();
        let price_rx = price_rx.clone();
        let mut sd_rx = shutdown_tx.subscribe();
        let telegram = telegram.clone();

        tokio::spawn(async move {
            info!("🤖 Bot executor iniciado.");
            let mut cycle_count = 0u64;
            loop {
                tokio::select! {
                    _ = sd_rx.recv() => {
                        info!("🔌 Desligando bot...");
                        let _ = telegram.send_alert("Shutdown", "Bot sendo desligado").await;
                        break;
                    },
                    result = async {
                        let mut rx = price_rx.lock().await;
                        rx.recv().await
                    } => {
                        if let Some(prices) = result {
                            cycle_count += 1;
                            if cycle_count % 10 == 0 {
                                debug!("📊 Ciclo #{} — {} DEXs", cycle_count, prices.len());
                            }
                            let mut bot_guard = bot.lock().await;
                            if let Err(e) = bot_guard.process_prices(prices).await {
                                error!("❌ Erro ao processar preços: {:?}", e);
                                let _ = telegram.send_error_alert("Processamento", &format!("{:?}", e)).await;
                            }
                        } else {
                            warn!("⚠️ Canal de preços fechado.");
                            tokio::time::sleep(Duration::from_secs(1)).await;
                        }
                    }
                }
            }
            info!("🛑 Executor encerrado após {} ciclos.", cycle_count);
        })
    };

    // ============================================================
    // 1️⃣1️⃣ Shutdown ordenado
    // ============================================================
    let shutdown_task = {
        let shutdown_tx = shutdown_tx.clone();
        let telegram = telegram.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                warn!("🛑 Ctrl-C recebido — encerrando...");
                let _ = telegram
                    .send_alert("Shutdown", "Ctrl-C recebido - encerrando bot")
                    .await;
                let _ = shutdown_tx.send(());
            }
        })
    };

    // ============================================================
    // 1️⃣2️⃣ TUI (Terminal User Interface)
    // ============================================================
    let tui_state = Arc::new(tokio::sync::RwLock::new(tui::TuiState::default()));
    let tui_state_clone = tui_state.clone();

    // Spawn TUI em thread separada
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let app = tui::TuiApp::new(tui_state_clone);
            if let Err(e) = app.run().await {
                eprintln!("TUI error: {:?}", e);
            }
        });
    });

    info!("🎯 TUI iniciado. Pressione 'q' no terminal da TUI para sair.");

    // ============================================================
    // 1️⃣3️⃣ Execução Concorrente e Debug
    // ============================================================

    // Configura a lista de tasks principais
    let tasks = vec![radar_task, bot_task, shutdown_task];

    info!("🎯 Sistema pronto (modo hot-reload).");

    if telegram.is_enabled() {
        let _ = telegram
            .send_alert("Sistema Pronto", "Bot iniciado e monitorando oportunidades")
            .await;
    }

    match future::try_join_all(tasks).await {
        Ok(_) => info!("✅ Todas as tasks finalizadas."),
        Err(e) => error!("❌ Erro nas tasks: {:?}", e),
    }

    metrics::set_bot_status(0);
    info!("👋 Encerrando Flashloan Bot com segurança.");
    let _ = telegram
        .send_alert("Bot Encerrado", "Finalizado com segurança")
        .await;

    Ok(())
}