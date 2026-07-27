// ============================================================
// src/main.rs — v4.8.4-HYBRID-SAFE (CORRIGIDO TYPE ERROR)
// ============================================================

use anyhow::{Context, Result};
use ethers::{
    providers::{Middleware, Provider, Ws},
};
use futures::future;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::Arc,
    time::{Duration, Instant},
};
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
        circuit_breaker::DexCircuitBreaker, manager::DexManager,
        radar::{compute_top_spreads, extract_edges, start_high_hit_rate_radar, AdjCostParams, TopSpreadInfo},
    },
    // execution:: imports removidos: ExecutionEngine/MevConfig/gwei eram codigo morto
    infra::{
        metrics,
        rpc_provider::{is_usable_endpoint, RpcProvider},
        try_serve_metrics_with_fallback,
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

/// Atualiza o estado compartilhado da TUI com os preços e a economia do ciclo atual.
fn update_tui_state(
    tui_state: &Arc<std::sync::RwLock<tui::TuiState>>,
    adj_cost: &AdjCostParams,
    prices: &HashMap<String, HashMap<String, f64>>,
    top_n: usize,
    cycle_count: u64,
    uptime: Duration,
) {
    let (_, _, economics, adj_cycles) = extract_edges(prices, adj_cost);

    // Net USD agregado: soma dos ciclos com net projetado > 0 (oportunidade real).
    let net_usd_total: f64 = adj_cycles
        .iter()
        .map(|c| c.net_profit_usd)
        .filter(|n| *n > 0.0)
        .sum();

    // Map canônico par → melhor net (p/ coluna Net$ da tabela de preços).
    // adj_cycles é deduped por canonical pair; norm_pair casa direção-agnóstica.
    let mut net_by_pair: HashMap<String, f64> = HashMap::new();
    for c in &adj_cycles {
        let key = tui::norm_pair(&c.pair);
        net_by_pair
            .entry(key)
            .and_modify(|v| {
                if c.net_profit_usd > *v {
                    *v = c.net_profit_usd;
                }
            })
            .or_insert(c.net_profit_usd);
    }

    let mut rows: HashMap<String, tui::PriceRow> = HashMap::new();
    for (dex, dex_map) in prices {
        for (pair, price) in dex_map {
            let row = rows.entry(pair.clone()).or_insert_with(|| tui::PriceRow {
                pair: pair.clone(),
                quickswap: None,
                sushiswap: None,
                curve: None,
                uniswap_v3: None,
                net_usd: net_by_pair.get(&tui::norm_pair(pair)).copied(),
            });
            match dex.as_str() {
                "QuickSwap" => row.quickswap = Some(*price),
                "SushiSwap" => row.sushiswap = Some(*price),
                "Curve" => row.curve = Some(*price),
                "UniswapV3" => row.uniswap_v3 = Some(*price),
                _ => {}
            }
        }
    }
    let mut last_prices: Vec<tui::PriceRow> = rows.into_values().collect();
    last_prices.sort_by(|a, b| a.pair.cmp(&b.pair));
    last_prices.truncate(20);

    // Top-N spreads (sync, sem TVL) — espelha o log [TOPSPREAD].
    let top_spreads: Vec<tui::TopSpreadRow> = compute_top_spreads(prices, adj_cost, top_n)
        .into_iter()
        .map(top_spread_row_from_info)
        .collect();

    if let Ok(mut state) = tui_state.write() {
        state.running = true;
        state.uptime = uptime;
        state.cycle_count = cycle_count;
        state.dex_count = prices.len();
        state.pairs_count = last_prices.len();
        state.gross_positive = economics.gross_positive as u32;
        state.net_positive = economics.net_positive as u32;
        state.negative_cycles = economics.negative_cycles_found as u32;
        state.net_usd_total = net_usd_total;
        state.top_spreads = top_spreads;
        state.last_prices = last_prices;
    }
}

/// Mapeia `TopSpreadInfo` (radar, sync) → `TopSpreadRow` (TUI, subset sem TVL).
fn top_spread_row_from_info(i: TopSpreadInfo) -> tui::TopSpreadRow {
    tui::TopSpreadRow {
        pair: i.pair,
        tui_spread_pct: i.tui_spread_pct,
        cycle_rate: i.cycle_rate,
        net_usd: i.net_usd,
        executable: i.executable,
        has_curve_leg: i.has_curve_leg,
        outlier: i.outlier,
    }
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

    // TUI ocupa terminal inteiro (alternate screen). Logs direto no stdout
    // colidem com o buffer da TUI e aparecem como texto solto fora das boxes.
    // Por isso logs vão sempre pra arquivo enquanto TUI roda.
    std::fs::create_dir_all("logs").context("❌ Falha ao criar diretório logs/")?;
    let log_file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open("logs/bot.log")
        .context("❌ Falha ao abrir logs/bot.log")?;

    if use_json_logs {
        tracing_subscriber::fmt()
            .json()
            .with_current_span(false)
            .with_target(false)
            .with_writer(std::sync::Mutex::new(log_file))
            .init();
    } else {
        let filter = tracing_subscriber::EnvFilter::builder()
            .with_default_directive(LevelFilter::INFO.into())
            .parse(env_filter)
            .context("❌ Falha ao parsear RUST_LOG")?;

        let writer = std::sync::Mutex::new(log_file).with_max_level(Level::INFO);

        let fmt_layer = fmt::layer()
            .compact()
            .with_ansi(false)
            .with_target(false)
            .with_writer(writer);

        tracing_subscriber::registry()
            .with(filter)
            .with(fmt_layer)
            .init();
    }

    info!("🧾 Logging habilitado em logs/bot.log (TUI usa terminal).");

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

    let config = Config::from_file(config_path.clone()).with_context(|| {
        format!(
            "❌ Falha ao ler o arquivo de configuração: {}",
            config_path.display()
        )
    })?;
    let cfg_unlocked = {
        let lock = config.lock().await;
        Arc::new(lock.clone())
    };

    // Custo config-driven p/ projetar net dos ciclos `adj` (log + TUI). Fonte única:
    // notional do [arbitrage].default_trade_amount, premium Aave V3 verificado
    // on-chain ([flashloan].fee_pct, 5 bps), gas base ([execution].estimate_base_gas_usd).
    // PROJEÇÃO — não é decisão de execução (desconto real é downstream em economics/flashloan).
    let adj_cost = Arc::new(AdjCostParams {
        notional_usd: cfg_unlocked
            .arbitrage
            .default_trade_amount
            .parse::<f64>()
            .unwrap_or(100.0),
        flashloan_fee_pct: cfg_unlocked.flashloan.fee_pct.unwrap_or(0.0005),
        gas_usd_est: cfg_unlocked.execution.estimate_base_gas_usd,
    });
    info!(
        "🧮 adj cost params | notional=${:.2} flashloan_fee_pct={:.5} gas_est=${:.4} → cost/cycle=${:.4}",
        adj_cost.notional_usd,
        adj_cost.flashloan_fee_pct,
        adj_cost.gas_usd_est,
        adj_cost.cost_usd(),
    );

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
    // 3️⃣.5 Servidor Prometheus (métricas)
    // ============================================================
    // Antes o servidor de métricas nunca era iniciado — Infrastructure::initialize()
    // não era chamado do main.rs, e toda a camada de observabilidade era decorativa.
    // Agora chamamos diretamente: inicia warp em [prometheus].port (fallback 9101).
    if cfg_unlocked.prometheus.enabled || cfg_unlocked.metrics.enabled {
        if let Err(e) = try_serve_metrics_with_fallback(&cfg_unlocked).await {
            warn!("⚠️ Falha ao iniciar servidor Prometheus: {}", e);
        }
    } else {
        info!("📊 Prometheus desativado via config.");
    }

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
    // 5️⃣ DexManager
    // ============================================================
    let dex_manager = Arc::new(
        DexManager::new(
            client_http.clone(),
            cfg_unlocked.clone(),
        )
        .await
        .context("❌ Falha ao inicializar DexManager")?,
    );
    info!("🧩 DexManager inicializado com sucesso.");

    // ============================================================
    // 6️⃣ ExecutionEngine removido (codigo morto)
    // ============================================================
    // ExecutionEngine/MevConfig/BundleSender eram construidos aqui e
    // descartados (_execution_engine). O bot executa via ArbitrageClient
    // diretamente (execute_direct/execute_flashloan/execute_wrapper).
    // Ver ESTADO_ATUAL.md secao 6.
    

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
    // 8️⃣.5 Replay scan (paper-only): varre blocos históricos e sai
    // ============================================================
    if flashloan_bot::core::replay_scan::should_run(&cfg_unlocked) {
        info!("📼 REPLAY_SCAN ativo — paper-only, sem radar/envio");
        let summary = {
            let guard = bot.lock().await;
            flashloan_bot::core::replay_scan::run(
                client_http.clone(),
                cfg_unlocked.clone(),
                guard.get_arbitrage_engine(),
                &guard.arbitrage_client,
            )
            .await
            .context("❌ replay_scan falhou")?
        };
        info!(
            target: "replay_scan",
            "📼 REPLAY_SCAN done | n={} edge={} sim_ok={} lucrativos={}",
            summary.n_blocos_amostrados,
            summary.n_blocos_com_edge,
            summary.n_sim_ok,
            summary.n_blocos_lucrativos_pos_custos
        );
        return Ok(());
    }

    // ============================================================
    // 9️⃣ Canais e Radar
    // ============================================================
    let (price_tx, price_rx) = mpsc::channel::<HashMap<String, HashMap<String, f64>>>(256);
    let price_rx = Arc::new(Mutex::new(price_rx));
    let (shutdown_tx, _) = broadcast::channel::<()>(4);

    let tui_state = Arc::new(std::sync::RwLock::new(tui::TuiState::default()));
    // Paper/headless: TUI precisa de TTY; skip quando PAPER_VALIDATION=1.
    let tui_enabled = !flashloan_bot::core::paper_validation::env_paper_flag();
    if tui_enabled {
        tui::spawn_tui(tui_state.clone());
    } else {
        info!("📄 PAPER mode: TUI desabilitado (headless)");
    }

    let radar_task = {
        let mut client_ws = client_ws.clone();
        let dex_manager = dex_manager.clone();
        let config = config.clone();
        let adj_cost = adj_cost.clone();
        let price_tx = price_tx.clone();
        let circuit_breaker = circuit_breaker.clone();
        let shutdown_tx = shutdown_tx.clone();
        let telegram = telegram.clone();

        tokio::spawn(async move {
            // start_high_hit_rate_radar só retorna Err quando a conexão WS
            // morre de vez (ex.: reconnect interno do ethers-rs esgotado —
            // ver logs/deployments do incidente 2026-07-27). Sem este loop,
            // uma única queda de WS matava o radar pro resto do processo:
            // o price_tx parava de receber, e a TUI (que roda em thread e
            // runtime próprios, ver src/tui.rs) continuava desenhando o
            // último estado congelado pra sempre, parecendo viva mas nunca
            // mais atualizando.
            const INITIAL_BACKOFF: Duration = Duration::from_secs(2);
            const MAX_BACKOFF: Duration = Duration::from_secs(60);
            let mut backoff = INITIAL_BACKOFF;

            loop {
                info!("📡 High Hit Rate Radar iniciado.");
                let sd_rx = shutdown_tx.subscribe();
                let result = start_high_hit_rate_radar(
                    client_ws.clone(),
                    dex_manager.clone(),
                    config.clone(),
                    adj_cost.clone(),
                    circuit_breaker.clone(),
                    price_tx.clone(),
                    sd_rx,
                )
                .await;

                match result {
                    // Encerramento limpo: o próprio radar já tratou o sinal
                    // de shutdown internamente e retornou Ok(()).
                    Ok(()) => break,
                    Err(e) => {
                        error!("❌ Radar erro: {:?}", e);
                        let _ = telegram
                            .send_error_alert("Radar", &format!("{:?}", e))
                            .await;

                        let mut sd_rx_wait = shutdown_tx.subscribe();
                        tokio::select! {
                            _ = sd_rx_wait.recv() => {
                                info!("📡 Radar: shutdown durante backoff — não reconecta.");
                                break;
                            }
                            _ = tokio::time::sleep(backoff) => {}
                        }
                        backoff = (backoff * 2).min(MAX_BACKOFF);

                        // A conexão antiga provavelmente está morta (reconnect
                        // interno já esgotado) — reconecta do zero, com o
                        // fallback já existente entre ws_endpoints, antes de
                        // tentar o radar de novo.
                        let net_cfg = { config.lock().await.network.clone() };
                        match RpcProvider::connect_ws(&net_cfg).await {
                            Ok(fresh) => {
                                info!("📡 Radar: WS reconectado, retomando.");
                                client_ws = fresh;
                                backoff = INITIAL_BACKOFF;
                            }
                            Err(e) => {
                                error!("❌ Radar: falha ao reconectar WS: {:?}", e);
                            }
                        }
                    }
                }
            }
            info!("📡 Radar: task encerrada.");
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
        let tui_state = tui_state.clone();
        let adj_cost = adj_cost.clone();
        let top_n = cfg_unlocked.log.top_spreads_n;
        let start_time = Instant::now();

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

                            update_tui_state(&tui_state, &adj_cost, &prices, top_n, cycle_count, start_time.elapsed());

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

    if tui_enabled {
        info!("🎯 TUI iniciado. Pressione 'q' no terminal da TUI para sair.");
    } else {
        info!("🎯 Headless (paper) — Ctrl-C para sair.");
    }

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