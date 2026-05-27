// ============================================================
// src/infra/metrics.rs — v4.4.4-FINAL-MICROPROFIT-SYNC
// ============================================================
// ✅ Compatível com Prometheus 0.13 e Warp 0.3
// ✅ Corrigido: macros e conflitos de registro duplicado
// ✅ Adicionado: DEX_REQUESTS, Flashloan Stats, HitRate
// ✅ Novo Gauge: LAST_GAS_USD (custo real de gas em USD)
// ✅ Servidor Prometheus funcional e assíncrono
// ✅ Integração total com RiskManager e Config (v4.4.4)
// ============================================================

use crate::config::Config;
use anyhow::Result;
use once_cell::sync::Lazy;
use prometheus::{
    register, register_counter, register_gauge, register_histogram, register_int_counter,
    register_int_counter_vec, register_int_gauge, Counter, Gauge, Histogram, IntCounter,
    IntCounterVec, IntGauge, Opts,
};
use std::{collections::HashMap, net::SocketAddr, sync::Mutex};
use tracing::{debug, info, warn};
use warp::Filter;

// ============================================================
// 🔧 REGISTROS PROMETHEUS GLOBAIS
// ============================================================

pub static BOT_START_TOTAL: Lazy<Counter> =
    Lazy::new(|| register_counter!("bot_start_total", "Número total de inicializações do bot").unwrap());

pub static BOT_STATUS: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("bot_status", "Status atual do bot (1 = ativo, 0 = inativo)").unwrap());

pub static ARBITRAGE_EXECUTIONS: Lazy<Counter> =
    Lazy::new(|| register_counter!("arbitrage_executions_total", "Número total de execuções de arbitragem").unwrap());

pub static LAST_PROFIT: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("last_profit_usd", "Lucro da última operação em USD").unwrap());

pub static LAST_GAS_USD: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("last_gas_usd", "Custo de gás da última operação em USD").unwrap());

pub static ERRORS_TOTAL: Lazy<Counter> =
    Lazy::new(|| register_counter!("errors_total", "Número total de erros registrados").unwrap());

pub static EXEC_LATENCY_MS: Lazy<Histogram> = Lazy::new(|| {
    register_histogram!(
        "exec_latency_ms",
        "Tempo de execução em milissegundos",
        vec![10.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0]
    )
    .unwrap()
});

pub static RISK_SCORE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("risk_score", "Pontuação de risco da operação").unwrap());

pub static RISK_REJECT: Lazy<Counter> =
    Lazy::new(|| register_counter!("risk_reject_total", "Número de rejeições por risco").unwrap());

pub static RISK_APPROVE: Lazy<Counter> =
    Lazy::new(|| register_counter!("risk_approve_total", "Número de aprovações por risco").unwrap());

pub static ADAPTIVE_APPROVE: Lazy<Counter> =
    Lazy::new(|| register_counter!("adaptive_approve_total", "Aprovações em modo adaptativo").unwrap());

pub static ADAPTIVE_MODE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("adaptive_mode", "Modo adaptativo ativo (1) ou inativo (0)").unwrap());

pub static HIT_RATE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("hit_rate_percent", "Taxa de acerto (%)").unwrap());

pub static SUCCESS_RATE: Lazy<Gauge> =
    Lazy::new(|| register_gauge!("success_rate", "Taxa de sucesso global (0–1)").unwrap());

pub static EXEC_OK: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("exec_ok_total", "Total de execuções bem-sucedidas").unwrap());

pub static DEX_REQUESTS: Lazy<IntCounter> =
    Lazy::new(|| register_int_counter!("dex_requests_total", "Total de requisições a DEXs").unwrap());

pub static EXEC_FAIL: Lazy<IntCounterVec> = Lazy::new(|| {
    let opts = Opts::new("exec_fail_total", "Total de execuções com falha (por motivo)");
    let vec = IntCounterVec::new(opts, &["reason"]).unwrap();
    register(Box::new(vec.clone())).ok();
    vec
});

// ============================================================
// ⚡️ MÉTRICAS FLASHLOAN
// ============================================================

use lazy_static::lazy_static;

lazy_static! {
    pub static ref FLASHLOAN_EXECUTIONS: IntCounter =
        register_int_counter!("flashloan_executions_total", "Total de execuções de flashloan").unwrap();

    pub static ref FLASHLOAN_PROFIT: Counter =
        register_counter!("flashloan_profit_usd", "Lucro total de flashloans em USD").unwrap();

    pub static ref FLASHLOAN_PREMIUM_PAID: Counter =
        register_counter!("flashloan_premium_paid_usd", "Premium total pago em flashloans em USD").unwrap();

    pub static ref FLASHLOAN_GAS_USAGE: Histogram = register_histogram!(
        "flashloan_gas_used",
        "Gas usado em execuções de flashloan",
        vec![100_000.0, 300_000.0, 600_000.0, 900_000.0, 1_200_000.0]
    )
    .unwrap();

    pub static ref ACTIVE_FLASHLOAN_MODE: IntGauge =
        register_int_gauge!("active_flashloan_mode", "Modo ativo (0=off, 1=wrapper, 2=aave)").unwrap();

    pub static ref COUNTER_DEX_REQUESTS: IntCounterVec = register_int_counter_vec!(
        "dex_requests_by_name_total",
        "Total de requisições para DEXs (por nome)",
        &["dex_name"]
    ).unwrap();
}

// ============================================================
// 🆕 Funções de controle de execução
// ============================================================

pub fn inc_exec_ok() {
    EXEC_OK.inc();
    debug!("✅ Execução registrada como bem-sucedida");
}

pub fn inc_exec_fail(reason: &str) {
    EXEC_FAIL.with_label_values(&[reason]).inc();
    warn!("❌ Execução com falha registrada: motivo = {reason}");
}

pub fn get_hit_rate() -> f64 {
    HIT_RATE.get()
}

// ============================================================
// 🧭 Funções gerais de métricas
// ============================================================

pub fn inc_bot_start_total() {
    BOT_START_TOTAL.inc();
}

pub fn set_bot_status(v: i64) {
    BOT_STATUS.set(v as f64);
}

pub fn inc_arbitrage_executions() {
    ARBITRAGE_EXECUTIONS.inc();
}

pub fn set_last_profit(v: f64) {
    LAST_PROFIT.set(v);
}

pub fn set_last_gas_usd(v: f64) {
    LAST_GAS_USD.set(v);
    debug!("⛽ Gas atualizado em métricas: ${:.6}", v);
}

pub fn inc_errors(context: &str) {
    ERRORS_TOTAL.inc();
    warn!("⚠️ Erro registrado em contexto: {context}");
}

pub fn observe_exec_latency_ms(latency: f64, ctx: &str) {
    EXEC_LATENCY_MS.observe(latency);
    debug!("⏱️ Execução ({ctx}) durou {:.2} ms", latency);
}

pub fn set_risk_score(v: f64) {
    RISK_SCORE.set(v);
}

pub fn inc_risk_reject() {
    RISK_REJECT.inc();
    warn!("🚫 Operação rejeitada pelo RiskManager");
}

pub fn inc_risk_approve() {
    RISK_APPROVE.inc();
}

pub fn inc_adaptive_approve() {
    ADAPTIVE_APPROVE.inc();
}

pub fn set_adaptive_mode(enabled: bool) {
    ADAPTIVE_MODE.set(if enabled { 1.0 } else { 0.0 });
}

pub fn set_hit_rate(rate: f64) {
    HIT_RATE.set(rate);
}

pub fn set_success_rate(v: f64) {
    SUCCESS_RATE.set(v);
}

// ============================================================
// ✅ Funções específicas para DEX
// ============================================================

pub fn inc_dex_request(dex_name: &str) {
    DEX_REQUESTS.inc();
    COUNTER_DEX_REQUESTS.with_label_values(&[dex_name]).inc();
    debug!("📊 Requisição DEX registrada: {}", dex_name);
}

// ============================================================
// ⚡ Flashloan Tracking
// ============================================================

pub fn record_flashloan_execution(mode: &str, profit: f64, premium: f64, gas_used: u64) {
    FLASHLOAN_EXECUTIONS.inc();
    FLASHLOAN_PROFIT.inc_by(profit);
    FLASHLOAN_PREMIUM_PAID.inc_by(premium);
    FLASHLOAN_GAS_USAGE.observe(gas_used as f64);

    let mode_val = match mode {
        "wrapper" => 1,
        "aave" => 2,
        _ => 0,
    };
    ACTIVE_FLASHLOAN_MODE.set(mode_val);
    debug!(
        "⚡ Flashloan registrado: modo={}, lucro={:.4}, premium={:.4}, gas={}",
        mode, profit, premium, gas_used
    );
}

// ============================================================
// 📊 Servidor Prometheus
// ============================================================

pub async fn serve_metrics(cfg: &Config) -> Result<()> {
    let port: u16 = cfg.metrics.port;
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    tokio::spawn(async move {
        info!("📈 Servidor Prometheus ativo em http://0.0.0.0:{}/metrics", port);

        let metrics_route = warp::path("metrics").map(|| {
            let body = prometheus::TextEncoder::new()
                .encode_to_string(&prometheus::gather())
                .unwrap_or_else(|_| "# erro ao gerar métricas".to_string());

            warp::reply::with_header(
                body,
                "content-type",
                "text/plain; version=0.0.4; charset=utf-8",
            )
        });

        warp::serve(metrics_route).run(addr).await;
    });

    Ok(())
}

// ============================================================
// 📊 Resumo e Exportação
// ============================================================

pub fn get_metrics_summary() -> String {
    format!(
        "📊 Execuções: {} | Flashloans: {} | Lucro Total: {:.4} USD | Gas Último: {:.6}",
        ARBITRAGE_EXECUTIONS.get(),
        FLASHLOAN_EXECUTIONS.get(),
        FLASHLOAN_PROFIT.get(),
        LAST_GAS_USD.get()
    )
}

// ============================================================
// 🧱 Contadores Dinâmicos
// ============================================================

static DYNAMIC_COUNTERS: Lazy<Mutex<HashMap<String, IntCounterVec>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));

pub fn inc_counter(name: &str) {
    let mut counters = DYNAMIC_COUNTERS.lock().unwrap();

    if !counters.contains_key(name) {
        let opts = Opts::new(name, format!("Contador dinâmico: {}", name));
        let vec = IntCounterVec::new(opts, &["event"]).unwrap();
        register(Box::new(vec.clone())).ok();
        counters.insert(name.to_string(), vec);
    }

    if let Some(vec) = counters.get(name) {
        vec.with_label_values(&["count"]).inc();
    }

    debug!("📈 Contador dinâmico incrementado: {}", name);
}
