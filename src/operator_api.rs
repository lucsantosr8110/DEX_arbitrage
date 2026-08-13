use axum::{extract::State, response::sse::{Event, KeepAlive, Sse}, routing::get, Json, Router};
use serde::Serialize;
use std::{convert::Infallible, net::SocketAddr, sync::{Arc, RwLock}, time::Duration};
use tokio::sync::broadcast;
use tokio_stream::{wrappers::BroadcastStream, StreamExt};
use tracing::{info, warn};

use crate::tui::TuiState;

#[derive(Clone)]
struct ApiState { tui: Arc<RwLock<TuiState>>, events: broadcast::Sender<String> }

#[derive(Serialize)]
struct Health { status: &'static str, api: &'static str, worker: &'static str, sequence: u64, data_age_ms: Option<u128> }

#[derive(Serialize)]
struct Snapshot {
    schema_version: &'static str, sequence: u64, generated_at: String,
    runtime: Runtime, safety: Safety, chain: Chain, round: Round,
}

#[derive(Serialize)] struct Runtime { mode: &'static str, dry_run: bool, phase: String, uptime_secs: u64, shutdown_state: &'static str }
#[derive(Serialize)] struct Safety { signer_present: bool, broadcaster_present: bool, wrapper_enabled: bool, simulate_before_execute: bool, economics_consistent: bool, mainnet_blocked: bool }
#[derive(Serialize)] struct Chain { network: &'static str, chain_id: u64, head_block: u64, anchor_block: u64, anchor_hash: &'static str, confirmations: u64, data_age_ms: Option<u128> }
#[derive(Serialize)] struct Round { duration_ms: u64, quotes: u64, edges: u64, cycles_detected: u64, routes_ranked: u64, routes_evaluated: u64, gross_positive: u64, economically_positive: u64, stable: u64, risk_approved: u64, selected: u64, timeouts: u64, latency_p50_ms: u64, latency_p95_ms: u64 }

pub async fn serve(tui: Arc<RwLock<TuiState>>, shutdown_tx: broadcast::Sender<()>) {
    let (events, _) = broadcast::channel(32);
    let state = ApiState { tui, events };
    let app = Router::new().route("/api/v1/health", get(health)).route("/api/v1/snapshot", get(snapshot)).route("/api/v1/events", get(operator_events)).with_state(state);
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
    let snapshot = build_snapshot(&state.tui);
    Json(Health { status: "ok", api: "ready", worker: if snapshot.round.quotes > 0 { "running" } else { "starting" }, sequence: snapshot.sequence, data_age_ms: state.tui.read().ok().and_then(|s| s.last_update.map(|i| i.elapsed().as_millis())) })
}

async fn snapshot(State(state): State<ApiState>) -> Json<Snapshot> { Json(build_snapshot(&state.tui)) }

async fn operator_events(State(state): State<ApiState>) -> Sse<impl tokio_stream::Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(state.events.subscribe()).filter_map(|item| item.ok()).map(|payload| Ok(Event::default().event("snapshot").data(payload)));
    Sse::new(stream).keep_alive(KeepAlive::new().interval(Duration::from_secs(15)).text("keep-alive"))
}

fn build_snapshot(tui: &Arc<RwLock<TuiState>>) -> Snapshot {
    let state = tui.read().expect("TUI state poisoned");
    let age = state.last_update.map(|instant| instant.elapsed().as_millis());
    Snapshot {
        schema_version: "operator.v1", sequence: state.cycle_count, generated_at: chrono::Utc::now().to_rfc3339(),
        runtime: Runtime { mode: "PAPER", dry_run: true, phase: state.startup_phase.clone(), uptime_secs: state.start.elapsed().as_secs(), shutdown_state: "armed" },
        safety: Safety { signer_present: false, broadcaster_present: false, wrapper_enabled: false, simulate_before_execute: true, economics_consistent: true, mainnet_blocked: true },
        chain: Chain { network: "Polygon", chain_id: 137, head_block: 0, anchor_block: 0, anchor_hash: "redacted", confirmations: 0, data_age_ms: age },
        round: Round { duration_ms: state.last_update.map(|i| i.elapsed().as_millis() as u64).unwrap_or(0), quotes: state.pairs_count as u64, edges: state.dex_count as u64, cycles_detected: state.negative_cycles as u64, routes_ranked: state.top_spreads.len() as u64, routes_evaluated: state.top_spreads.len() as u64, gross_positive: state.gross_positive as u64, economically_positive: state.net_positive as u64, stable: 0, risk_approved: 0, selected: 0, timeouts: 0, latency_p50_ms: 0, latency_p95_ms: 0 },
    }
}
