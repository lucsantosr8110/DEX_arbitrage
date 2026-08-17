//! Smoke test end-to-end: operador API serve histórico persistido em SQLite.
//! Sobe `operator_api::serve` com um DB temporário, insere rodadas e valida
//! `/api/v1/rounds` + `/api/v1/stats` via HTTP cru (sem dep de cliente HTTP).

use flashloan_bot::{
    infra::history::{RoundHistory, RoundRecord},
    operator_api,
    tui::TuiState,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

fn record(sequence: u64) -> RoundRecord {
    RoundRecord {
        sequence,
        completed_at: format!("2026-08-14T12:00:{:02}Z", sequence % 60),
        duration_ms: Some(1_000),
        discovery_ms: Some(600),
        shadow_ms: Some(300),
        quotes: 8,
        edges: None,
        cycles_detected: 121,
        routes_ranked: 6,
        routes_evaluated: None,
        gross_positive: 1,
        economically_positive: 0,
        stable: Some(0),
        risk_approved: Some(0),
        selected: Some(0),
        net_usd_total: 0.42,
        anchor_block: Some(100),
        best_route_kind: Some("two_leg".into()),
        best_route_path: Some("WMATIC→USDC".into()),
        best_route_venues: Some("QuickSwap / UniswapV3".into()),
        best_route_gross: Some(1.5),
        best_route_net: Some(0.42),
        tui_spread_pct: Some(1.5),
        cycle_rate_pct: Some(0.42),
        cycle_net_usd: Some(0.42),
        anchor_resolution_ms: Some(10),
        metadata_ms: Some(20),
        quote_ms: Some(300),
        ranking_ms: Some(15),
        requote_ms: Some(400),
        context_build_ms: Some(25),
        materialization_economics_ms: Some(50),
        unattributed_ms: Some(5),
        best_route_gross_pnl_usd: Some(0.5),
        best_route_gas_cost_usd: Some(0.3),
        best_route_flashloan_cost_usd: Some(0.1),
        best_route_negative_cause: Some("GAS_DOMINATES".into()),
    }
}

async fn http_get(port: u16, path: &str) -> String {
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .expect("conectar na API");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("enviar request");
    let mut body = Vec::new();
    stream.read_to_end(&mut body).await.expect("ler response");
    String::from_utf8_lossy(&body).to_string()
}

#[tokio::test]
async fn rounds_and_stats_endpoints_serve_persisted_history() {
    let port = 18_099;
    std::env::set_var("OPERATOR_API_PORT", port.to_string());
    let db_path = std::env::temp_dir().join(format!("api-history-{}.db", std::process::id()));
    let _ = std::fs::remove_file(&db_path);

    let db = RoundHistory::open(&db_path).expect("abrir histórico");
    let tui = Arc::new(std::sync::RwLock::new(TuiState::default()));
    let (shutdown_tx, _rx) = tokio::sync::broadcast::channel::<()>(1);
    let tui2 = tui.clone();
    let db2 = db.clone();
    tokio::spawn(async move { operator_api::serve(tui2, Some(db2), shutdown_tx, None).await; });
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    db.insert_round(&record(1)).expect("insert round 1");
    db.insert_round(&record(2)).expect("insert round 2");

    let rounds_body = http_get(port, "/api/v1/rounds?limit=5").await;
    assert!(
        rounds_body.contains("\"sequence\":2"),
        "rounds deve ter seq 2 primeiro: {rounds_body}"
    );
    assert!(rounds_body.contains("\"sequence\":1"));
    assert!(rounds_body.contains("WMATIC→USDC"));

    let stats_body = http_get(port, "/api/v1/stats").await;
    assert!(
        stats_body.contains("\"total_rounds\":2"),
        "stats total_rounds: {stats_body}"
    );
    assert!(stats_body.contains("\"best_net_usd\":0.42"));
    assert!(stats_body.contains("\"history_enabled\":true"));
    assert!(
        stats_body.contains("\"duration_p50_ms\":1000"),
        "stats latency p50: {stats_body}"
    );
    assert!(stats_body.contains("\"duration_p95_ms\":1000"));
    assert!(stats_body.contains("\"discovery_p50_ms\":600"));

    let _ = std::fs::remove_file(&db_path);
}
