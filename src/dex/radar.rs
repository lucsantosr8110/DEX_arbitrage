// ============================================================
// src/dex/radar.rs — v5.3.0 (M3 ULTRA-LIQUIDO, RÁPIDO e COMPATÍVEL)
// ============================================================
// Apenas pares realmente executáveis na Polygon (2025)
// 30 pares — stables + WMATIC + WETH
// Sem tokens mortos, sem noise, sem wBTC.
// Execução ultra rápida, baixa CPU e mínimo load Alchemy
// ============================================================

use crate::{
    config::Config,
    core::smart_retry::SmartRetryManager,
    dex::{
        circuit_breaker::DexCircuitBreaker,
        manager::DexManager,
        price_cache::PriceCache,
        rate_limiter::{ALCHEMY_RATE_LIMITER, DEX_RATE_LIMITER},
    },
};
use chrono::Utc;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::StreamExt;
use anyhow::{Result, anyhow};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::Write as IoWrite,
    path::Path,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{broadcast, mpsc, Mutex},
    task,
};
use tracing::{error, warn, info, instrument};

// ============================================================
// TOKEN LISTS (M3 — ULTRA LÍQUIDOS)
// ============================================================
//
// Apenas tokens que realmente possuem pools profundos no QuickSwap,
// SushiSwap e Uniswap V3 em 2025.
//
// ============================================================

const STABLES: &[&str] = &["USDC", "USDT", "DAI"];
const BLUECHIPS: &[&str] = &["WMATIC", "WETH"];

// ============================================================
// NORMALIZAÇÃO
// ============================================================

fn normalize_token(t: &str) -> &str {
    match t.to_uppercase().as_str() {
        "MATIC" => "WMATIC",
        "WMATIC" => "WMATIC",
        "WETH" => "WETH",
        "USDC" => "USDC",
        "USDT" => "USDT",
        "DAI" => "DAI",
        _ => t,
    }
}

// ============================================================
// CANONICAL PAIR NAME
// ============================================================

fn canonical_pair_name(a: &str, b: &str) -> String {
    let a = normalize_token(a);
    let b = normalize_token(b);

    let a_stable = STABLES.contains(&a);
    let b_stable = STABLES.contains(&b);

    if a_stable && !b_stable {
        return format!("{}-{}", a, b);
    }
    if b_stable && !a_stable {
        return format!("{}-{}", b, a);
    }
    if a < b {
        format!("{}-{}", a, b)
    } else {
        format!("{}-{}", b, a)
    }
}

/// 🔧 NOVO: par DIRECIONAL (sem ordenar / sem stable-first)
/// Isso é CRÍTICO para o ArbitrageEngine, que procura pair e reverse_pair.
fn pair_name_directional(a: &str, b: &str) -> String {
    let a = normalize_token(a);
    let b = normalize_token(b);
    format!("{}-{}", a, b)
}

// ============================================================
// MATRIZ ULTRA-LIQUIDA (M3 — 30 pares)
// ============================================================

fn curated_matrix_pairs() -> Vec<String> {
    let mut set = std::collections::HashSet::new();

    // Stable ↔ Stable
    for a in STABLES {
        for b in STABLES {
            if a != b {
                set.insert(canonical_pair_name(a, b));
            }
        }
    }

    // Stable ↔ Bluechips
    for s in STABLES {
        for b in BLUECHIPS {
            set.insert(canonical_pair_name(s, b));
        }
    }

    // Bluechip ↔ Bluechip
    set.insert(canonical_pair_name("WMATIC", "WETH"));

    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

// ============================================================
// MERGE COM CONFIG (se o usuário adicionar mais pares)
// ============================================================

fn generate_full_pair_list(cfg: &Config) -> Vec<String> {
    let mut set = std::collections::HashSet::new();

    // Matriz M3
    for p in curated_matrix_pairs() {
        set.insert(p);
    }

    // Config do usuário
    for p in &cfg.pairs.monitor {
        if let Some((a, b)) = p.split_once('-') {
            set.insert(canonical_pair_name(a, b));
        }
    }

    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

// ============================================================
// QUICK FILTER — filtragem inteligente por spread real
// ============================================================

#[derive(Clone)]
struct HighHitRateFilter {
    enabled: bool,
    min_spread_percent: f64,
}

impl HighHitRateFilter {
    fn new(enabled: bool, min: f64) -> Self {
        Self {
            enabled,
            min_spread_percent: min.max(0.001),
        }
    }

    #[instrument(skip_all)]
    fn should_analyze_dex_pair(
        &self,
        dex_name: &str,
        canon_pair: &str,
        previous: &HashMap<String, HashMap<String, f64>>,
        cb: &DexCircuitBreaker,
    ) -> bool {
        if cb.should_skip(dex_name) {
            return false;
        }
        if !self.enabled {
            return true;
        }

        let Some(cur) = previous.get(dex_name) else { return true };
        let Some(&prev_price) = cur.get(canon_pair) else { return true };

        // Média dos outros DEX
        let mut others = vec![];
        for (d, map) in previous {
            if d == dex_name {
                continue;
            }
            if let Some(&p) = map.get(canon_pair) {
                others.push(p);
            }
        }

        if others.is_empty() {
            return true;
        }

        let avg = others.iter().sum::<f64>() / others.len() as f64;
        let spread = ((avg - prev_price).abs() / prev_price) * 100.0;

        spread >= self.min_spread_percent
    }
}

// ============================================================
// ADAPTERS SAUDÁVEIS
// ============================================================

async fn get_healthy_adapters(dm: &Arc<DexManager>, cb: &DexCircuitBreaker) -> Vec<String> {
    dm.get_healthy_adapters()
        .await
        .into_iter()
        .filter(|n| !cb.should_skip(n))
        .collect()
}

// ============================================================
// MULTICALL
// ============================================================

#[instrument(skip_all)]
async fn collect_dex_prices(
    adapter: String,
    pairs: Vec<String>,
    qf: HighHitRateFilter,
    previous: HashMap<String, HashMap<String, f64>>,
    dm: Arc<DexManager>,
    cb: Arc<DexCircuitBreaker>,
    retry: SmartRetryManager,
) -> Result<(String, HashMap<String, f64>)> {

    let mut filtered = vec![];
    for p in pairs {
        if qf.should_analyze_dex_pair(&adapter, &p, &previous, &cb) {
            filtered.push(p);
        }
    }

    if filtered.is_empty() {
        return Ok((adapter, HashMap::new()));
    }

    let result = retry
        .exec("multicall", || {
            let dm2 = dm.clone();
            let ad = adapter.clone();
            let batch = filtered.clone();
            Box::pin(async move { dm2.get_prices_multicall(&ad, &batch).await })
        })
        .await?;

    let mut out = HashMap::new();

    for tp in result {
        // O canonical é ótimo para reduzir noise e comparar spreads “por par”.
        // Mas o ENGINE precisa do par direcional também (pair e reverse_pair).
        let canon = canonical_pair_name(&tp.token_a, &tp.token_b);

        let a = normalize_token(&tp.token_a).to_string();
        let b = normalize_token(&tp.token_b).to_string();

        // Direcional A->B e B->A
        let ab = pair_name_directional(&a, &b);
        let ba = pair_name_directional(&b, &a);

        // Validação simples (mantida)
        if !(tp.price.is_finite()) {
            continue;
        }
        if tp.price <= 0.0 || tp.price >= 1_000_000_000.0 {
            continue;
        }

        // ✅ Inserir canonical (para filtros e auditoria)
        out.insert(canon, tp.price);

        // ✅ Inserir direcional A->B
        out.insert(ab, tp.price);

        // ✅ Inserir reverso B->A (inverso matemático)
        let inv = 1.0 / tp.price;
        if inv.is_finite() && inv > 0.0 && inv < 1_000_000_000.0 {
            out.insert(ba, inv);
        }
    }

    Ok((adapter, out))
}

// ============================================================
// CONTAGEM DE "SINAIS" (não confundir com oportunidades do ENGINE)
// ============================================================

fn count_spread_signals(pr: &HashMap<String, HashMap<String, f64>>) -> usize {
    let mut map_pairs: HashMap<String, Vec<f64>> = HashMap::new();

    for (_, dex_map) in pr {
        for (pair, price) in dex_map {
            // Observação: aqui contamos “pares com spread”, não “rotas executáveis”.
            map_pairs.entry(pair.clone()).or_default().push(*price);
        }
    }

    let mut count = 0;

    for (_, vals) in map_pairs {
        if vals.len() < 2 {
            continue;
        }

        let min = vals.iter().copied().fold(f64::MAX, f64::min);
        let max = vals.iter().copied().fold(f64::MIN, f64::max);

        if min <= 0.0 {
            continue;
        }

        let spread = (max - min) / min * 100.0;
        if spread >= 0.03 {
            count += 1;
        }
    }

    count
}

// ============================================================
// LOG DE AUDITORIA
// ============================================================

fn log_price_audit(pr: &HashMap<String, HashMap<String, f64>>, cycle: u64) {
    if pr.is_empty() {
        return;
    }

    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    fs::create_dir_all("audits").ok();
    let fp = "audits/prices_audit.csv";
    let exists = Path::new(fp).exists();

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(fp) {
        if !exists {
            let _ = writeln!(f, "timestamp,cycle,dex,pair,price");
        }
        for (dex, m) in pr {
            for (pair, price) in m {
                let _ = writeln!(f, "{},{},{},{},{:.8}", ts, cycle, dex, pair, price);
            }
        }
    }
}

// ============================================================
// EXECUÇÃO DO CICLO
// ============================================================

#[instrument(skip_all)]
async fn execute_radar_cycle(
    dm: &Arc<DexManager>,
    cfg: &Arc<Mutex<Config>>,
    price_tx: &mpsc::Sender<HashMap<String, HashMap<String, f64>>>,
    previous: &mut HashMap<String, HashMap<String, f64>>,
    cb: Arc<DexCircuitBreaker>,
    retry: &SmartRetryManager,
    cycle: u64,
) -> Result<(usize, usize)> {

    let (pairs, qf_enabled, min_spread) = {
        let cfg = cfg.lock().await;
        (
            generate_full_pair_list(&cfg),
            cfg.optimization.quick_filter_enabled,
            cfg.optimization.min_spread_percent,
        )
    };

    let adapters = get_healthy_adapters(dm, &cb).await;
    let qf = HighHitRateFilter::new(qf_enabled, min_spread);

    let mut tasks = task::JoinSet::new();

    for ad in adapters {
        let prev = previous.clone();
        let batch = pairs.clone();
        let dm2 = dm.clone();
        let cb2 = cb.clone();
        let retry2 = retry.clone();
        let qf2 = qf.clone();

        tasks.spawn(async move {
            collect_dex_prices(ad, batch, qf2, prev, dm2, cb2, retry2).await
        });
    }

    let mut out = HashMap::new();

    while let Some(res) = tasks.join_next().await {
        match res {
            Ok(Ok((dex, map))) => {
                if !map.is_empty() {
                    out.insert(dex, map);
                }
            }
            Ok(Err(e)) => warn!("Erro no DEX: {:?}", e),
            Err(e) => warn!("Join error: {:?}", e),
        }
    }

    let total: usize = out.values().map(|m| m.len()).sum();
    let signals = count_spread_signals(&out);

    log_price_audit(&out, cycle);

    *previous = out.clone();

    if total > 0 {
        price_tx.send(out).await?;
    }

    Ok((total, signals))
}

// ============================================================
// LOOP PRINCIPAL DO RADAR
// ============================================================

#[instrument(skip_all)]
pub async fn start_high_hit_rate_radar(
    ws: Arc<Provider<Ws>>,
    dm: Arc<DexManager>,
    cfg: Arc<Mutex<Config>>,
    price_cache: Arc<PriceCache>,
    cb: Arc<DexCircuitBreaker>,
    price_tx: mpsc::Sender<HashMap<String, HashMap<String, f64>>>,
    mut shutdown_rx: broadcast::Receiver<()>
) -> Result<()> {

    let mut stream = ws.subscribe_blocks().await?;
    let retry = SmartRetryManager::new(2, Duration::from_millis(35)).with_jitter(0.2);
    let mut previous: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut cycle = 0u64;

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                warn!("Radar encerrado");
                break;
            }

            Some(_) = stream.next() => {
                cycle += 1;

                ALCHEMY_RATE_LIMITER.cleanup().await;
                DEX_RATE_LIMITER.cleanup().await;

                match execute_radar_cycle(
                    &dm, &cfg, &price_tx,
                    &mut previous, cb.clone(),
                    &retry, cycle
                ).await {
                    Ok((total, signals)) =>
                        info!("Ciclo {} — {} preços, {} sinais-spread", cycle, total, signals),
                    Err(e) =>
                        error!("Erro no ciclo {}: {:?}", cycle, e),
                }
            }
        }
    }

    Ok(())
}
