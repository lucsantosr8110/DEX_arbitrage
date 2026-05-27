use axum::{extract::State, routing::get, Json, Router};
use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, sync::Arc};
use tokio::sync::Mutex;
use tracing::info;

use crate::config::Config;

/// Estado compartilhado (config, dados em tempo real, etc.)
pub type SharedState = Arc<Mutex<Config>>;

/// Estrutura usada para updates vindos do frontend
#[derive(Debug, Deserialize)]
pub struct UpdateConfig {
    pub log_level: Option<String>,
}

/// Estrutura de status geral (pra dashboard React)
#[derive(Debug, Serialize)]
pub struct StatusResponse {
    pub running: bool,
    pub connected_network: String,
    pub active_dex: Vec<String>,
}

/// Estrutura de preços mockada (placeholder até integrar radar realtime)
#[derive(Debug, Serialize)]
pub struct PricesResponse {
    pub pair: String,
    pub price: f64,
}

/// Inicia o servidor do frontend (REST API para React)
pub async fn serve_frontend(config: SharedState) {
    let app = Router::new()
        .route("/config", get(get_config).post(update_config))
        .route("/status", get(get_status))
        .route("/prices", get(get_prices))
        .with_state(config);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("🌍 Frontend disponível em http://{}", addr);

    axum::serve(
        tokio::net::TcpListener::bind(addr).await.unwrap(),
        app.into_make_service(),
    )
    .await
    .unwrap();
}

/// GET /config → retorna configuração atual
async fn get_config(State(config): State<SharedState>) -> Json<Config> {
    let cfg = config.lock().await;
    Json(cfg.clone())
}

/// POST /config → atualiza parâmetros da config
async fn update_config(
    State(config): State<SharedState>,
    Json(update): Json<UpdateConfig>,
) -> Json<Config> {
    let mut cfg = config.lock().await;
    if let Some(level) = update.log_level {
        cfg.logging.level = level;
    }
    Json(cfg.clone())
}

/// GET /status → retorna informações básicas do bot
async fn get_status(State(config): State<SharedState>) -> Json<StatusResponse> {
    let cfg = config.lock().await;
    Json(StatusResponse {
        running: true,
        connected_network: cfg.network.name.clone(),
        active_dex: cfg
            .dex
            .iter()
            .filter(|d| d.enabled)
            .map(|d| d.name.clone())
            .collect(),
    })
}

/// GET /prices → mock, pode ser substituído pelo radar em tempo real
async fn get_prices() -> Json<Vec<PricesResponse>> {
    Json(vec![
        PricesResponse {
            pair: "WMATIC/WETH".to_string(),
            price: 0.0054,
        },
        PricesResponse {
            pair: "WETH/USDC".to_string(),
            price: 2450.12,
        },
    ])
}
