// ============================================================
// src/infra/mod.rs — v4.8.7-REAL-MICRO-STABLE
// ============================================================
//
// Compatível com metrics.rs (v4.8.x)
// Removido prometheus_export() obsoleto
// Inclui TextEncoder direto (sem função externa)
// Fallback automático: porta 9100 → 9101 → 9102
// TokenCache Global e Telegram integrados
// ============================================================

pub mod metrics;
pub mod network;
pub mod rpc_provider;
pub mod rotating_http_client;
pub mod price_feed;

use anyhow::Result;
use std::{net::SocketAddr, sync::Arc};
use tracing::{debug, info, warn};

use crate::{
    config::{token_cache::TokenCache, Config},
    infra::rpc_provider::RpcProvider,
    utils::telegram::TelegramNotifier,
};

// ============================================================
// Estrutura principal da infraestrutura
// ============================================================

pub struct Infrastructure {
    pub http_client: Option<Arc<crate::AppMiddleware>>,
    pub ws_client: Option<Arc<ethers::providers::Provider<ethers::providers::Ws>>>,
    pub config: Arc<Config>,
}

impl Infrastructure {
    // ============================================================
    // Inicialização completa
    // ============================================================
    pub async fn initialize(cfg: Arc<Config>) -> Result<Self> {
        info!(
            "{} | Inicializando infraestrutura (v4.8.7)...",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );

        // TokenCache global
        let token_cache = TokenCache::global(cfg.clone()).await;
        let cache_size = token_cache.size().await;
        info!(
            "{} | TokenCache global inicializado com {} tokens.",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            cache_size
        );

        // Providers HTTP e WS
        let mut http_client: Option<Arc<crate::AppMiddleware>> = None;
        let mut ws_client: Option<Arc<ethers::providers::Provider<ethers::providers::Ws>>> = None;

        // Evitar erro de tipo em Option<str> (usa String)
        let wallet_key = cfg.wallet.private_key.clone();
        if !wallet_key.is_empty() {
            match RpcProvider::connect_http(&cfg.network, &wallet_key).await {
                Ok(client) => {
                    info!("Provider HTTP inicializado com sucesso.");
                    http_client = Some(client);
                }
                Err(e) => warn!("Falha ao conectar HTTP provider: {e}"),
            }
        } else {
            warn!("Chave privada vazia - HTTP provider não será inicializado.");
        }

        match RpcProvider::connect_ws(&cfg.network).await {
            Ok(ws) => {
                info!("WebSocket conectado com sucesso.");
                ws_client = Some(ws);
            }
            Err(e) => warn!("Falha ao conectar WebSocket: {e}"),
        }

        // Métricas Prometheus (com fallback). Infrastructure::initialize é
        // camada legada (main.rs usa try_serve_metrics_with_fallback direto
        // com o shutdown_tx do processo); criamos um canal local só p/ satisfazer
        // a assinatura — o servidor aqui spawned sem sinal real de shutdown.
        if cfg.metrics.enabled {
            let (local_tx, _) = tokio::sync::broadcast::channel::<()>(4);
            if let Err(e) = try_serve_metrics_with_fallback(&cfg, local_tx).await {
                warn!("Falha ao iniciar métricas Prometheus: {e}");
            }
        } else {
            info!("Métricas Prometheus desativadas via config.");
        }

        // Telegram Notifier
        if cfg.telegram.as_ref().map_or(false, |tg_cfg| tg_cfg.enabled) {
            match TelegramNotifier::init_from_config(&cfg).await {
                Ok(_) => info!("Telegram ativo e pronto para alertas."),
                Err(e) => warn!("Falha ao inicializar Telegram: {e}"),
            }
        } else {
            debug!("Telegram desativado via configuração.");
        }

        info!(
            "{} | Infraestrutura totalmente inicializada.",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S")
        );

        Ok(Self {
            http_client,
            ws_client,
            config: cfg,
        })
    }

    // ============================================================
    // Acessores
    // ============================================================

    pub fn http(&self) -> Option<Arc<crate::AppMiddleware>> {
        self.http_client.clone()
    }

    pub fn ws(&self) -> Option<Arc<ethers::providers::Provider<ethers::providers::Ws>>> {
        self.ws_client.clone()
    }
}

// ============================================================
// Servidor Prometheus com fallback automático
// ============================================================

pub async fn try_serve_metrics_with_fallback(cfg: &Config, shutdown_tx: tokio::sync::broadcast::Sender<()>) -> Result<()> {
    use crate::infra::metrics::{inc_bot_start_total, set_bot_status};

    // Preferir [prometheus].port (alinhado com prometheus.yml) quando ativo;
    // cair para [metrics].port como fallback.
    let base_port = if cfg.prometheus.enabled && cfg.prometheus.port > 0 {
        cfg.prometheus.port
    } else {
        cfg.metrics.port
    };
    let mut port = base_port;

    // registra métricas base
    inc_bot_start_total();
    set_bot_status(1);

    for attempt in 0..3 {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();

        match start_metrics_server(addr, shutdown_tx.clone()).await {
            Ok(_) => {
                info!(
                    "{} | Prometheus ativo em http://0.0.0.0:{}/metrics",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                    port
                );
                return Ok(());
            }
            Err(e) => {
                warn!(
                    "{} | Porta {} indisponível: {} — tentando próxima... (tentativa {}/{})",
                    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
                    port,
                    e,
                    attempt + 1,
                    3
                );
                port += 1;
            }
        }
    }

    Err(anyhow::anyhow!(
        "Falha ao iniciar servidor Prometheus após múltiplas tentativas"
    ))
}

// ============================================================
// Inicializador real do servidor Prometheus
// ============================================================

async fn start_metrics_server(addr: SocketAddr, shutdown_tx: tokio::sync::broadcast::Sender<()>) -> Result<()> {
    use prometheus::{Encoder, TextEncoder};
    use warp::Filter;

    info!(
        "{} | Iniciando servidor Prometheus em {}...",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
        addr
    );

    let routes = warp::path("metrics").map(|| {
        let encoder = TextEncoder::new();
        let metric_families = prometheus::gather();
        let mut buffer = Vec::new();

        if let Err(e) = encoder.encode(&metric_families, &mut buffer) {
            warn!("Erro ao gerar métricas Prometheus: {:?}", e);
        }

        let body = String::from_utf8(buffer)
            .unwrap_or_else(|_| "# erro ao converter métricas".to_string());

        warp::reply::with_header(body, "content-type", "text/plain; version=0.0.4")
    });

    // Rodar servidor em thread assíncrona com graceful shutdown: antes
    // warp::serve().run(addr) nunca retornava — task leaked, porta ficava
    // presa até o processo morrer (reinício pegava EADDRINUSE às vezes).
    // Agora select! entre run() e o broadcast de shutdown: quando o sinal
    // chega, o future run() é droppado e o listener fecha.
    tokio::spawn(async move {
        info!(
            "{} | Servidor Prometheus aguardando conexões em {}",
            chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
            addr
        );
        let mut rx = shutdown_tx.subscribe();
        let server = warp::serve(routes).run(addr);
        tokio::select! {
            _ = server => {}
            _ = rx.recv() => {
                info!("📈 Servidor Prometheus: shutdown recebido, fechando listener {}.", addr);
            }
        }
    });

    // Pequeno atraso para garantir binding da porta
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    info!(
        "{} | Prometheus ativo em http://127.0.0.1:{}/metrics",
        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S"),
        addr.port()
    );

    Ok(())
}
