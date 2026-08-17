use axum::{
    extract::{Query, State},
    response::sse::{Event, KeepAlive, Sse},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    convert::Infallible,
    net::SocketAddr,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tracing::{info, warn};

use crate::{
    dex::circuit_breaker::DexCircuitBreaker,
    infra::history::{OverallStats, RoundHistory, RoundRow},
    tui::TuiState,
};

#[derive(Clone)]
struct ApiState {
    tui: Arc<RwLock<TuiState>>,
    history: Option<Arc<RoundHistory>>,
    events: broadcast::Sender<String>,
    circuit_breaker: Option<Arc<DexCircuitBreaker>>,
}

#[derive(Serialize)]
struct Health { status: &'static str, api: &'static str, worker: &'static str, sequence: u64, data_age_ms: Option<u128> }

#[derive(Serialize)]
struct Snapshot {
    schema_version: &'static str, sequence: u64, generated_at: String,
    data_source: &'static str, runtime: Runtime, safety: Safety, chain: Chain, round: Round,
    prices: Vec<Price>, routes: Vec<Route>, rpc: Vec<Rpc>, alerts: Vec<Alert>,
}

#[derive(Serialize)] struct Runtime { mode: &'static str, dry_run: bool, phase: String, uptime_secs: u64, shutdown_state: &'static str }
#[derive(Serialize)] struct Safety { signer_present: bool, broadcaster_present: bool, wrapper_enabled: bool, simulate_before_execute: bool, economics_consistent: Option<bool>, mainnet_blocked: bool }
#[derive(Serialize)] struct Chain { network: &'static str, chain_id: u64, head_block: Option<u64>, anchor_block: Option<u64>, anchor_hash: Option<&'static str>, confirmations: Option<u64>, data_age_ms: Option<u128> }
#[derive(Serialize)] struct Round { duration_ms: Option<u64>, quotes: u64, edges: Option<u64>, cycles_detected: u64, routes_ranked: u64, routes_evaluated: Option<u64>, gross_positive: u64, economically_positive: u64, stable: Option<u64>, risk_approved: Option<u64>, selected: Option<u64>, timeouts: Option<u64>, latency_p50_ms: Option<u64>, latency_p95_ms: Option<u64> }
#[derive(Serialize)] struct Price { pair: String, quickswap: Option<f64>, sushiswap: Option<f64>, curve: Option<f64>, uniswap_v3: Option<f64>, net_usd: Option<f64> }
#[derive(Serialize)] struct Route { id: String, route_kind: &'static str, path: String, venues: String, gross: f64, net: Option<f64>, distance: f64, status: &'static str, authoritative: bool, executable: bool, reason: Option<String>, gross_pnl_usd: Option<f64>, gas_cost_usd: Option<f64>, flashloan_cost_usd: Option<f64> }
#[derive(Serialize)] struct Rpc { alias: String, status: &'static str, latency_ms: Option<u64>, failures: u32, cooldown_active: bool }
#[derive(Serialize)] struct Alert { severity: &'static str, title: String, detail: String }

#[derive(Serialize)]
struct RoundsResponse { rounds: Vec<RoundRow> }

#[derive(Serialize)]
struct StatsResponse { history_enabled: bool, stats: OverallStats }

#[derive(Deserialize)]
struct RoundsQuery { limit: Option<u64> }

pub async fn serve(
    tui: Arc<RwLock<TuiState>>,
    history: Option<Arc<RoundHistory>>,
    shutdown_tx: broadcast::Sender<()>,
    circuit_breaker: Option<Arc<DexCircuitBreaker>>,
) {
    let (events, _) = broadcast::channel(32);
    let state = ApiState { tui, history, events, circuit_breaker };
    let app = Router::new()
        .route("/api/v1/health", get(health))
        .route("/api/v1/snapshot", get(snapshot))
        .route("/api/v1/rounds", get(rounds))
        .route("/api/v1/stats", get(stats))
        .route("/api/v1/events", get(operator_events))
        .with_state(state);
    let port = std::env::var("OPERATOR_API_PORT").ok().and_then(|v| v.parse().ok()).unwrap_or(8080);
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match tokio::net::TcpListener::bind(addr).await { Ok(listener) => listener, Err(error) => { warn!(%error, "operator API não iniciou"); return; } };
    info!("Operator API read-only em http://{addr}/api/v1");
    let mut shutdown = shutdown_tx.subscribe();
    tokio::select! {
        result = axum::serve(listener, app) => { if let Err(error) = result { warn!(%error, "operator API encerrou com erro"); } }
        _ = shutdown.recv() => info!("Operator API: shutdown recebido"),
    }
}

async fn health(State(state): State<ApiState>) -> Json<Health> {
    let snapshot = build_snapshot(&state.tui, &state.history, &state.circuit_breaker);
    Json(Health { status: "ok", api: "ready", worker: if snapshot.sequence > 0 { "running" } else { "starting" }, sequence: snapshot.sequence, data_age_ms: state.tui.read().ok().and_then(|s| s.last_update.map(|i| i.elapsed().as_millis())) })
}

async fn snapshot(State(state): State<ApiState>) -> Json<Snapshot> { Json(build_snapshot(&state.tui, &state.history, &state.circuit_breaker)) }

async fn rounds(State(state): State<ApiState>, Query(query): Query<RoundsQuery>) -> Json<RoundsResponse> {
    let limit = query.limit.unwrap_or(24).clamp(1, 500);
    let rounds = match &state.history {
        Some(db) => db.recent_rounds(limit).unwrap_or_default(),
        None => Vec::new(),
    };
    Json(RoundsResponse { rounds })
}

async fn stats(State(state): State<ApiState>) -> Json<StatsResponse> {
    let history_enabled = state.history.is_some();
    let stats = match &state.history {
        Some(db) => db.overall_stats().unwrap_or_default(),
        None => OverallStats::default(),
    };
    Json(StatsResponse { history_enabled, stats })
}

async fn operator_events(State(state): State<ApiState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| item.ok()).map(|payload| Ok(Event::default().event("snapshot").data(payload)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
}

fn build_snapshot(
    tui: &Arc<RwLock<TuiState>>,
    history: &Option<Arc<RoundHistory>>,
    circuit_breaker: &Option<Arc<DexCircuitBreaker>>,
) -> Snapshot {
    let state = tui.read().expect("TUI state poisoned");
    let age = state.last_update.map(|instant| instant.elapsed().as_millis());
    // Latência p50/p95 da duração do round, calculada do histórico SQLite.
    let latency = history
        .as_ref()
        .and_then(|db| db.overall_stats().ok())
        .map(|stats| stats.latency);
    // Circuit breaker state por adapter. Snapshot via `list_states()` é async —
    // coletamos de forma bloqueante via `futures::executor::block_on` (mesmo
    // padrão já usado em `should_skip` em circuit_breaker.rs).
    let cb_states: Vec<(String, u32, bool)> = match circuit_breaker {
        Some(cb) => futures::executor::block_on(cb.list_states()),
        None => Vec::new(),
    };
    Snapshot {
        schema_version: "operator.v1", sequence: state.cycle_count, generated_at: chrono::Utc::now().to_rfc3339(), data_source: "tui_state",
        runtime: Runtime { mode: "PAPER", dry_run: std::env::var("CONFIG_FILE").map(|v| v.contains("dryrun")).unwrap_or(true), phase: state.startup_phase.clone(), uptime_secs: state.start.elapsed().as_secs(), shutdown_state: "armed" },
        safety: Safety { signer_present: false, broadcaster_present: false, wrapper_enabled: false, simulate_before_execute: true, economics_consistent: state.economics_consistent, mainnet_blocked: true },
        chain: Chain { network: "Polygon", chain_id: 137, head_block: None, anchor_block: None, anchor_hash: None, confirmations: None, data_age_ms: age },
        round: Round { duration_ms: None, quotes: state.pairs_count as u64, edges: None, cycles_detected: state.negative_cycles as u64, routes_ranked: state.top_spreads.len() as u64, routes_evaluated: None, gross_positive: state.gross_positive as u64, economically_positive: state.net_positive as u64, stable: None, risk_approved: None, selected: None, timeouts: None, latency_p50_ms: latency.as_ref().and_then(|l| l.duration_p50_ms), latency_p95_ms: latency.as_ref().and_then(|l| l.duration_p95_ms) },
        prices: state.last_prices.iter().map(|price| Price { pair: price.pair.clone(), quickswap: price.quickswap, sushiswap: price.sushiswap, curve: price.curve, uniswap_v3: price.uniswap_v3, net_usd: price.net_usd }).collect(),
        routes: state.top_spreads.iter().enumerate().map(|(index, route)| Route { id: format!("{}-{:02}", state.cycle_count, index + 1), route_kind: if route.hop_count >= 3 { "triangular" } else { "two_leg" }, path: route.legs_label.clone().unwrap_or_else(|| route.pair.clone()), venues: format!("{} / {}", route.buy_dex, route.sell_dex), gross: route.tui_spread_pct, net: route.net_usd, distance: route.distance_to_profit, status: "observed", authoritative: false, executable: false, reason: route.outlier.clone().or_else(|| Some("observação read-only; sem autorização de execução".into())), gross_pnl_usd: route.gross_pnl_usd, gas_cost_usd: route.gas_cost_usd, flashloan_cost_usd: route.flashloan_cost_usd }).collect(),
        rpc: cb_states.iter().map(|(name, failures, cooldown_active)| Rpc {
            alias: name.clone(),
            status: if *cooldown_active { "cooldown" } else if *failures > 0 { "degraded" } else { "healthy" },
            latency_ms: None,
            failures: *failures,
            cooldown_active: *cooldown_active,
        }).collect(),
        alerts: Vec::new(),
    }
}
