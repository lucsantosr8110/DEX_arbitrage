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
use anyhow::Result;
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
use tracing::{debug, error, info, instrument, warn};

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

/// Tokens cujos pools na Polygon foram confirmados com liquidez real.
/// WBTC re-incluído após correção do endereço (B2 gate corta dust).
const KNOWN_LIQUID: &[&str] = &[
    "USDC", "USDT", "DAI", "WMATIC", "WETH", "WBTC", "LINK", "UNI", "LDO",
];

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
        "WBTC" | "BTC" => "WBTC",
        "LINK" => "LINK",
        "UNI" => "UNI",
        "LDO" => "LDO",
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

/// Emite pares **direcionais**: para cada par lógico A-B, tanto `"A-B"` quanto `"B-A"`.
///
/// Se `pairs.monitor` estiver preenchido, essa lista é a **única** fonte (universo
/// curado paper-first). A matriz M3 só entra quando `monitor` está vazio.
fn generate_full_pair_list(cfg: &Config) -> Vec<String> {
    let mut canonical = std::collections::HashSet::new();

    let allow: Vec<String> = if cfg.pairs.liquidity_allowlist.is_empty() {
        KNOWN_LIQUID.iter().map(|s| s.to_string()).collect()
    } else {
        cfg.pairs
            .liquidity_allowlist
            .iter()
            .map(|s| normalize_token(s).to_string())
            .collect()
    };
    let allowed = |t: &str| allow.iter().any(|a| a == t);

    if cfg.pairs.monitor.is_empty() {
        // Fallback: matriz ultra-líquida M3
        for p in curated_matrix_pairs() {
            canonical.insert(p);
        }
    } else {
        for p in &cfg.pairs.monitor {
            if let Some((a, b)) = p.split_once('-') {
                let na = normalize_token(a);
                let nb = normalize_token(b);
                if allowed(na) && allowed(nb) {
                    canonical.insert(canonical_pair_name(a, b));
                } else {
                    debug!(
                        "⏭️ par {}-{} ignorado: token fora do allowlist de liquidez",
                        a, b
                    );
                }
            }
        }
    }

    // Expande cada par canônico nas duas direções reais.
    let mut set = std::collections::HashSet::new();
    for p in &canonical {
        if let Some((a, b)) = p.split_once('-') {
            set.insert(pair_name_directional(a, b));
            set.insert(pair_name_directional(b, a));
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

    // Gate de liquidez (TVL proxy) — multicall balanceOf no mesmo ciclo do batch.
    // Threshold do venue (`[[dex]].liquidity_threshold_usd`) com fallback global.
    let min_liq =
        crate::dex::liquidity::min_pool_liquidity_usd_for_dex(dm.config_ref(), &adapter);
    let n_before = result.len();
    let result = dm
        .filter_prices_by_liquidity(&adapter, result, min_liq)
        .await?;
    let low_liq_cut = n_before.saturating_sub(result.len());
    if low_liq_cut > 0 {
        info!(
            target: "liquidity",
            dex = %adapter,
            discarded = low_liq_cut,
            kept = result.len(),
            min_usd = min_liq,
            total = crate::dex::liquidity::low_liquidity_discarded_count(),
            "liquidity gate: pares cortados por TVL proxy < threshold"
        );
    }

    let mut out = HashMap::new();

    for tp in result {
        // Só entra cotação REAL, na direção em que foi cotada.
        //
        // Antes, o reverso B->A era gravado como 1/preço. `getAmountsOut` e
        // `quoteExactInputSingle` já embutem fee e price impact; o inverso os
        // apaga. Num triângulo cross-DEX isso vira p1/p2 — lucro aparente sempre
        // que dois DEXs divergem, sem arbitragem nenhuma por trás. O
        // ArbitrageEngine já se recusa a fabricar inversos pelo mesmo motivo
        // (ver core/arbitrage.rs:1009); o radar é que estava furando a regra.
        //
        // As duas direções agora são cotadas separadamente — ver
        // `generate_full_pair_list`.
        if !tp.price.is_finite() || tp.price <= 0.0 || tp.price >= 100_000.0 {
            continue;
        }

        let a = normalize_token(&tp.token_a).to_string();
        let b = normalize_token(&tp.token_b).to_string();

        out.insert(pair_name_directional(&a, &b), tp.price);
    }

    prune_non_reciprocal(&adapter, &mut out);

    Ok((adapter, out))
}

// ============================================================
// GATE DE RECIPROCIDADE
// ============================================================

/// Janela aceitável para `p(A,B) × p(B,A)` dentro do mesmo DEX.
///
/// Ida e volta num par real perde duas vezes a taxa da pool, então o produto fica
/// *abaixo* de 1,0 — cerca de 0,994 para V2 a 30 bps, 0,999 para V3 a 5 bps. O piso
/// de 0,95 tolera pools rasas, onde o price impact soma ao fee. O teto ligeiramente
/// acima de 1,0 é folga para ruído: as duas pontas não são cotadas atomicamente, o
/// bloco pode virar entre elas.
///
/// Produto fora da janela significa cotação corrompida — decimais errados, pool
/// morto ou rota sintética. Na auditoria, `GRT-WETH × WETH-GRT` dava 3,3e-7.
const RECIPROCITY_MIN: f64 = 0.95;
const RECIPROCITY_MAX: f64 = 1.01;

/// Liquidez mínima do pool: **fonte única** = `config.arbitrage.min_liquidity` (USD).
/// Ver `dex::liquidity::min_pool_liquidity_usd`. (Constante morta removida.)

/// Spread máximo realista entre DEXes. Acima disso é dust pool ou erro de oracle.
const MAX_SPREAD_PCT: f64 = 50.0;

/// Zona de auditoria: edges com spread entre estes valores são logados em warn!
/// para investigação manual, mas NÃO são descartados.
const AUDIT_SPREAD_LOW: f64 = 10.0;
const AUDIT_SPREAD_HIGH: f64 = 50.0;

/// Derruba os dois lados de todo par cujo produto recíproco esteja fora da janela.
///
/// Derruba ambos de propósito: quando o produto está errado não dá para saber qual
/// das duas pontas mentiu, e manter a "boa" seria escolher no escuro.
fn prune_non_reciprocal(dex: &str, map: &mut HashMap<String, f64>) {
    let mut doomed: Vec<String> = Vec::new();

    for (pair, price) in map.iter() {
        let Some((a, b)) = pair.split_once('-') else {
            continue;
        };
        let reverse = pair_name_directional(b, a);

        // Avalia cada par uma vez só, pela direção lexicograficamente menor.
        if pair.as_str() > reverse.as_str() {
            continue;
        }

        let Some(rev_price) = map.get(&reverse) else {
            // Sem o reverso não há como validar; não derruba.
            // O engine exige as duas pontas para arbitrar (evaluate_direct).
            continue;
        };

        let product = price * rev_price;
        if !(RECIPROCITY_MIN..=RECIPROCITY_MAX).contains(&product) {
            // debug, não warn: para pools cronicamente rasos (DAI na Polygon, p.ex.)
            // isso dispara todo ciclo e viraria centenas de linhas por minuto. O
            // descarte é o comportamento correto e esperado, não uma anomalia — quem
            // investiga preço liga RUST_LOG=debug. Ver testes em `mod tests`.
            debug!(
                "🚫 [{}] recíproco fora da janela: {}={:.10} × {}={:.10} → {:.6} (janela {:.2}–{:.2}). Ambos descartados.",
                dex, pair, price, reverse, rev_price, product, RECIPROCITY_MIN, RECIPROCITY_MAX
            );
            doomed.push(pair.clone());
            doomed.push(reverse);
        }
    }

    for k in doomed {
        map.remove(&k);
    }
}

// ============================================================
// CONTAGEM DE "SINAIS" (não confundir com oportunidades do ENGINE)
// ============================================================

/// Dados de um edge cross-DEX detectado num ciclo.
pub struct EdgeInfo {
    pub pair: String,
    pub buy_dex: String,
    pub sell_dex: String,
    pub spread_pct: f64,
    pub buy_price: f64,
    pub sell_price: f64,
}

/// Estatísticas econômicas do ciclo (resumo para o log).
pub struct CycleEconomics {
    /// Pares avaliados (com ≥2 DEXes e reverso disponível).
    pub evaluated: usize,
    /// Pares onde `cycle_rate = buy×sell` (quotes fee-inclusive) > 1.0.
    pub gross_positive: usize,
    /// Mesmo critério que `gross_positive` (legado TUI). Quotes já embutem fee AMM;
    /// não há segunda dedução de fee venue no radar.
    pub venue_fee_adjusted_positive: usize,
    /// Pares com cycle_rate < 1.0 (sem oportunidade).
    pub negative_cycles_found: usize,
}

/// Conta sinais de spread E extrai os edges (> 0.01%) para logging/audit.
///
/// **VALIDAÇÃO CROSS-DEX**: Só emite EDGE quando `cycle_rate = buy_price × sell_price`
/// (quotes já fee-inclusive via getAmountsOut / Quoter) é > 1.0.
/// Spreads single-direction (ex: USDT-WMATIC 2.42% sem reverso viável) são filtrados.
pub fn extract_edges(pr: &HashMap<String, HashMap<String, f64>>) -> (usize, Vec<EdgeInfo>, CycleEconomics) {
    let mut map_pairs: HashMap<String, Vec<(String, f64)>> = HashMap::new();

    for (dex, dex_map) in pr {
        for (pair, price) in dex_map {
            map_pairs
                .entry(pair.clone())
                .or_default()
                .push((dex.clone(), *price));
        }
    }

    let mut count = 0;
    let mut edges = Vec::new();
    let mut evaluated = 0usize;
    let mut gross_positive = 0usize;
    let mut fee_adjusted_positive = 0usize;
    let mut negative_cycles = 0usize;

    for (pair, dex_prices) in &map_pairs {
        if dex_prices.len() < 2 {
            continue;
        }

        // Extrair reverse pair (ex: USDT-WMATIC → WMATIC-USDT)
        let reverse_pair = pair.split_once('-').map(|(a, b)| format!("{}-{}", b, a));

        // Buscar preços do reverse pair se existir
        let reverse_dex_prices = reverse_pair.as_ref().and_then(|rp| map_pairs.get(rp));

        // Calcular melhor cycle_rate cross-DEX com fees
        let mut best_cycle_rate: f64 = 0.0;
        let mut best_buy_dex = String::new();
        let mut best_sell_dex = String::new();
        let mut best_buy_price: f64 = 0.0;
        let mut best_sell_price: f64 = 0.0;

        if let Some(rev_prices) = reverse_dex_prices {
            // Para cada combinação (buy_dex, sell_dex) onde buy ≠ sell
            for (buy_dex, buy_price) in dex_prices {
                for (sell_dex, sell_price) in rev_prices {
                    if buy_dex == sell_dex {
                        continue;
                    }
                    // Quotes (getAmountsOut / Quoter) já incluem fee AMM + impact.
                    // NÃO aplicar (1-fee) de novo — isso era dedução dupla.
                    let cycle_rate = buy_price * sell_price;
                    if cycle_rate > best_cycle_rate {
                        best_cycle_rate = cycle_rate;
                        best_buy_dex = buy_dex.clone();
                        best_sell_dex = sell_dex.clone();
                        best_buy_price = *buy_price;
                        best_sell_price = *sell_price;
                    }
                }
            }
        }

        if best_cycle_rate == 0.0 {
            // Sem reverso disponível — não há como avaliar ciclo
            continue;
        }

        evaluated += 1;

        // Contabilizar economia do ciclo (gross == fee-inclusive product)
        if best_cycle_rate > 1.0 {
            gross_positive += 1;
            fee_adjusted_positive += 1;
        } else {
            negative_cycles += 1;
        }

        // Só emitir EDGE se cycle_rate > 1.0 (potencial bruto positivo)
        if best_cycle_rate <= 1.0 {
            debug!(
                "🚫 [NO-EDGE] {} cycle_rate={:.6} ≤ 1.0 — spread single-direction, não arbitrável",
                pair, best_cycle_rate
            );
            continue;
        }

        // Spread real reflete o cycle_rate, não o spread single-direction
        let spread_pct = (best_cycle_rate - 1.0) * 100.0;

        // Zona de auditoria: spreads entre 10-50% são logados para investigação
        if spread_pct >= AUDIT_SPREAD_LOW && spread_pct <= AUDIT_SPREAD_HIGH {
            warn!(
                "🔍 [AUDIT] {} spread={:.2}% ({}→{}) buy={:.6} sell={:.6} cycle_rate={:.6} — verificar liquidez",
                pair, spread_pct, best_buy_dex, best_sell_dex, best_buy_price, best_sell_price, best_cycle_rate
            );
        }

        if spread_pct > MAX_SPREAD_PCT {
            continue;
        }
        if spread_pct >= 0.03 {
            count += 1;
        }
        if spread_pct >= 0.01 {
            edges.push(EdgeInfo {
                pair: pair.clone(),
                buy_dex: best_buy_dex,
                sell_dex: best_sell_dex,
                spread_pct,
                buy_price: best_buy_price,
                sell_price: best_sell_price,
            });
        }
    }

    edges.sort_by(|a, b| {
        b.spread_pct
            .partial_cmp(&a.spread_pct)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let economics = CycleEconomics {
        evaluated,
        gross_positive,
        venue_fee_adjusted_positive: fee_adjusted_positive,
        negative_cycles_found: negative_cycles,
    };

    (count, edges, economics)
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
// LOG DE EDGES CROSS-DEX
// ============================================================

/// Log INFO com resumo dos edges detectados no ciclo (formato compacto).
fn log_edge_summary(edges: &[EdgeInfo], cycle: u64) {
    if edges.is_empty() {
        return;
    }

    let best = &edges[0];
    let avg: f64 = edges.iter().map(|e| e.spread_pct).sum::<f64>() / edges.len() as f64;

    // Log compacto em uma linha
    let top3: Vec<String> = edges
        .iter()
        .take(3)
        .map(|e| format!("{} {:.4}% {}→{}", e.pair, e.spread_pct, e.buy_dex, e.sell_dex))
        .collect();

    info!(
        "📈 EDGE #{:03} | {:2} edges | avg {:.4}% | TOP: {} {:.4}% {}→{} | {}",
        cycle,
        edges.len(),
        avg,
        best.pair,
        best.spread_pct,
        best.buy_dex,
        best.sell_dex,
        top3.join(" | ")
    );
}

/// Escreve edges no CSV de auditoria para analise posterior.
fn log_edge_audit(edges: &[EdgeInfo], cycle: u64) {
    if edges.is_empty() {
        return;
    }

    let ts = Utc::now().format("%Y-%m-%d %H:%M:%S").to_string();

    fs::create_dir_all("audits").ok();
    let fp = "audits/edges_audit.csv";
    let exists = Path::new(fp).exists();

    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(fp) {
        if !exists {
            let _ = writeln!(f, "timestamp,cycle,pair,buy_dex,sell_dex,spread_pct,buy_price,sell_price");
        }
        for e in edges {
            let _ = writeln!(
                f,
                "{},{},{},{},{},{:.6},{:.8},{:.8}",
                ts, cycle, e.pair, e.buy_dex, e.sell_dex, e.spread_pct, e.buy_price, e.sell_price
            );
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
) -> Result<(usize, Vec<EdgeInfo>)> {

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
                } else {
                    // AUDIT 2026-07-25: DEX saudável mas 0 cotações válidas nesta
                    // rodada (ex.: Curve quando o pair set não tem stable-stable).
                    // Antes isto era silencioso — a DEX simplesmente sumia do
                    // `dex_count` sem log. Agora é barulhento para distinguir
                    // "sem pool p/ estes pares" de "falha de init/RPC".
                    warn!(
                        "🔻 DEX {} retornou 0 cotações — excluída do resumo (pair set sem pares suportados?)",
                        dex
                    );
                }
            }
            Ok(Err(e)) => warn!("Erro no DEX: {:?}", e),
            Err(e) => warn!("Join error: {:?}", e),
        }
    }

    let total: usize = out.values().map(|m| m.len()).sum();
    let (_signals, edges, economics) = extract_edges(&out);

    log_price_audit(&out, cycle);
    log_edge_summary(&edges, cycle);
    log_edge_audit(&edges, cycle);

    // Log de resultado econômico do ciclo
    info!(
        "💰 Resultado econômico | evaluated={} gross_positive={} venue_fee_adjusted_positive={} negative_cycles_found={}",
        economics.evaluated, economics.gross_positive, economics.venue_fee_adjusted_positive, economics.negative_cycles_found
    );

    // Log de preços cross-DEX para diagnóstico
    if !out.is_empty() {
        let mut pair_prices: HashMap<String, Vec<(&String, &f64)>> = HashMap::new();
        for (dex, dex_map) in &out {
            for (pair, price) in dex_map {
                pair_prices.entry(pair.clone()).or_default().push((dex, price));
            }
        }
        // Mostra apenas pares com cotação de ≥2 DEXes (candidatos a arbitragem)
        let mut cross_dex: Vec<_> = pair_prices.iter()
            .filter(|(_, prices)| prices.len() >= 2)
            .collect();
        cross_dex.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

        for (pair, prices) in cross_dex.iter().take(8) {
            let mut parts: Vec<String> = prices.iter()
                .map(|(dex, price)| format!("{}={:.8}", dex, price))
                .collect();
            parts.sort();
            info!("  📈 {} | {}", pair, parts.join(" | "));
        }
        if cross_dex.len() > 8 {
            info!("  ... e mais {} pares cross-DEX", cross_dex.len() - 8);
        }
    }

    *previous = out.clone();

    if total > 0 {
        price_tx.send(out).await?;
    }

    Ok((total, edges))
}

// ============================================================
// LOOP PRINCIPAL DO RADAR
// ============================================================

#[instrument(skip_all)]
pub async fn start_high_hit_rate_radar(
    ws: Arc<Provider<Ws>>,
    dm: Arc<DexManager>,
    cfg: Arc<Mutex<Config>>,
    _price_cache: Arc<PriceCache>,
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
                info!("📡 Radar encerrado após {} ciclos", cycle);
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
                    Ok((total, edges)) =>
                        debug!("Ciclo {} — {} preços, {} sinais-spread, {} edges logados", cycle, total, edges.len(), edges.len()),
                    Err(e) =>
                        error!("Erro no ciclo {}: {:?}", cycle, e),
                }
            }
        }
    }

    Ok(())
}

// ============================================================
// TESTES
// ============================================================
#[cfg(test)]
mod tests {
    use super::*;

    fn m(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    #[test]
    fn reciprocidade_mantem_par_v2_saudavel() {
        // Ida e volta V2 a 30 bps: produto ~0,994, dentro da janela.
        let mut map = m(&[("USDC-USDT", 0.997), ("USDT-USDC", 0.997)]);
        prune_non_reciprocal("QuickSwap", &mut map);
        assert!(map.contains_key("USDC-USDT"));
        assert!(map.contains_key("USDT-USDC"));
    }

    #[test]
    fn reciprocidade_derruba_cotacao_corrompida() {
        // Caso real da auditoria: GRT-WETH × WETH-GRT ≈ 3,3e-7.
        let mut map = m(&[("GRT-WETH", 2.823e-12), ("WETH-GRT", 117_828.8)]);
        prune_non_reciprocal("QuickSwap", &mut map);
        assert!(map.is_empty(), "ambos os lados devem cair");
    }

    #[test]
    fn reciprocidade_derruba_pool_raso_para_o_notional() {
        // Pool que só aguenta muito menos que o notional: ~9% por perna.
        let mut map = m(&[("DAI-WMATIC", 11.66), ("WMATIC-DAI", 0.0757)]);
        prune_non_reciprocal("SushiSwap", &mut map);
        assert!(map.is_empty());
    }

    #[test]
    fn reciprocidade_ignora_direcao_unica() {
        // Sem o reverso não há como validar; não derruba (o engine é que
        // exige as duas pontas para arbitrar).
        let mut map = m(&[("USDC-WETH", 0.00053)]);
        prune_non_reciprocal("UniswapV3", &mut map);
        assert!(map.contains_key("USDC-WETH"));
    }

    #[test]
    fn pares_monitor_curado_nao_injeta_matriz_m3() {
        let mut cfg = Config::default();
        cfg.pairs.liquidity_allowlist = vec![
            "USDC".into(),
            "USDT".into(),
            "DAI".into(),
            "WETH".into(),
            "WMATIC".into(),
            "WBTC".into(),
        ];
        cfg.pairs.monitor = vec![
            "USDC-WETH".into(),
            "USDC-WMATIC".into(),
            "USDT-WETH".into(),
            "WBTC-USDC".into(),
            "WMATIC-WETH".into(),
        ];
        let pares = generate_full_pair_list(&cfg);
        // 5 lógicos × 2 direções = 10; sem injeção USDT-DAI da matriz M3
        assert_eq!(pares.len(), 10, "pares={:?}", pares);
        assert!(!pares.iter().any(|p| p == "USDT-DAI" || p == "DAI-USDT"));
        assert!(pares.contains(&"USDC-WETH".into()));
        assert!(pares.contains(&"WETH-USDC".into()));
        assert!(pares.contains(&"WBTC-USDC".into()));
    }

    #[test]
    fn pares_gerados_sao_direcionais_e_no_allowlist() {
        // Sem pares de config: só a matriz curada, expandida nas duas direções.
        let cfg = Config::default();
        let pares = generate_full_pair_list(&cfg);
        // Toda entrada tem a forma A-B com A,B no allowlist de liquidez.
        for p in &pares {
            let (a, b) = p.split_once('-').expect("par bem formado");
            assert!(KNOWN_LIQUID.contains(&a), "{} fora do allowlist", a);
            assert!(KNOWN_LIQUID.contains(&b), "{} fora do allowlist", b);
        }
        // Direcional: se A-B existe, B-A também.
        for p in &pares {
            let (a, b) = p.split_once('-').unwrap();
            let rev = format!("{}-{}", b, a);
            assert!(pares.contains(&rev), "falta reverso de {}", p);
        }
    }

    /// Quotes fee-inclusive: cycle_rate == buy_price × sell_price (sem (1-fee)).
    #[test]
    fn extract_edges_cycle_rate_equals_product_v2x_v2() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // buy Sushi A-B=1.01, sell Quick B-A=1.0 → cycle=1.01 → spread 1%
        // Se ainda deduzisse (0.997)^2, spread cairia ~0.4%.
        pr.insert(
            "QuickSwap".into(),
            m(&[("AAA-BBB", 1.0), ("BBB-AAA", 1.0)]),
        );
        pr.insert(
            "SushiSwap".into(),
            m(&[("AAA-BBB", 1.01), ("BBB-AAA", 1.0)]),
        );

        let (_n, edges, econ) = extract_edges(&pr);
        assert!(!edges.is_empty(), "deve emitir edge");
        let best = &edges[0];
        assert!(
            (best.spread_pct - 1.0).abs() < 1e-9,
            "spread esperado 1.0%, got {}",
            best.spread_pct
        );
        assert_eq!(econ.gross_positive, econ.venue_fee_adjusted_positive);
    }

    /// Mesmo produto fee-inclusive cross-DEX V3×V3 (sem escala de fee no cycle).
    #[test]
    fn extract_edges_cycle_rate_equals_product_v3x_v3() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        pr.insert(
            "UniswapV3".into(),
            m(&[("CCC-DDD", 1.005), ("DDD-CCC", 1.0)]),
        );
        // Segundo "venue" sintético: usamos Sushi só como perna sell (preços fee-inclusive).
        // Para V3×V3 puro precisaríamos 2 mapas UniswapV3 — o radar exige buy_dex != sell_dex
        // por nome, então o segundo mapa simula outro venue com quotes já inclusive.
        pr.insert(
            "QuickSwap".into(),
            m(&[("CCC-DDD", 1.0), ("DDD-CCC", 1.0)]),
        );

        let (_n, edges, _) = extract_edges(&pr);
        assert!(!edges.is_empty());
        // Melhor: 1.005 * 1.0 = 1.005 → 0.5%
        assert!((edges[0].spread_pct - 0.5).abs() < 1e-9);
    }

    /// Curve (am3CRV) só cota stable-stable. Se o `monitor` não tiver
    /// USDC-USDT / DAI-USDC / DAI-USDT, a coluna Curve fica 100% vazia e a
    /// DEX cai do `dex_count` (silent drop, ver fix radar.rs). Este teste fixa
    /// o requisito: monitor com stable-stable → pair set direcional inclui os
    /// pares que Curve atende. Previne regressão (remover stable-stable = mata
    /// Curve silenciosamente).
    #[test]
    fn monitor_com_stable_stable_gera_pares_direcionais_para_curve() {
        let mut cfg = Config::default();
        cfg.pairs.liquidity_allowlist = vec!["USDC".into(), "USDT".into(), "DAI".into()];
        cfg.pairs.monitor = vec![
            "USDC-USDT".into(),
            "DAI-USDC".into(),
            "DAI-USDT".into(),
        ];
        let pares = generate_full_pair_list(&cfg);

        // Curve precisa das DUAS direções (get_dy(i,j) e get_dy(j,i)).
        for esperado in [
            "USDC-USDT", "USDT-USDC",
            "DAI-USDC", "USDC-DAI",
            "DAI-USDT", "USDT-DAI",
        ] {
            assert!(pares.contains(&esperado.to_string()), "falta {esperado} (Curve não cota)");
        }
    }
}
