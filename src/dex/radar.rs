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
    core::{
        replay_cross_model::{route_all_legs_executable, venue_curve_model, CurveModel},
        smart_retry::SmartRetryManager,
    },
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

/// Uma perna de um ciclo `adj` (round-trip 2-hop cross-DEX num par).
pub struct AdjLeg {
    pub venue: String,
    pub token_in: String,
    pub token_out: String,
    /// Preço fee-inclusive (tokenOut por tokenIn) vindo do quote (getAmountsOut/Quoter/get_dy).
    pub rate: f64,
}

/// Ciclo que passou o ajuste de custo (`adj` = `venue_fee_adjusted_positive`,
/// incrementado quando `cycle_rate > 1.0`). Emitido por `extract_edges` p/ o
/// caller fazer persistência + log estruturado. **Nota:** neste layer do radar
/// `adj == gross` — quotes já embutem fee AMM; o desconto de gas/flashloan fee
/// acontece downstream (economics/flashloan). O flag `executable` distingue
/// "caixa acionável" de "vitrine" (ex.: perna Curve, sem DexType on-chain).
pub struct AdjCycleInfo {
    /// Chave determinística (independente do scan): join de "venue|in>out" por perna.
    pub cycle_key: String,
    pub pair: String,
    pub legs: Vec<AdjLeg>,
    pub cycle_rate: f64,
    /// (cycle_rate - 1) * 100.
    pub gross_profit_pct: f64,
    pub executable: bool,
    pub has_curve_leg: bool,
}

/// Estado de persistência de um ciclo `adj` entre scans.
#[derive(Clone, Copy, Debug)]
pub struct AdjSeen {
    pub first_seen_scan: u64,
    pub last_seen_scan: u64,
    /// Quantos scans CONSECUTIVOS o ciclo apareceu como `adj`. Quebra se ausente
    /// num scan (last_seen != scan-1 na próxima aparição).
    pub seen_consecutive: usize,
}

/// Mapa em memória {cycle_key -> AdjSeen}, atualizado a cada scan. Responde
/// "por quantos ciclos de scan consecutivos o mesmo ciclo apareceu como adj".
pub struct AdjTracker {
    map: HashMap<String, AdjSeen>,
    current_scan: u64,
}

impl Default for AdjTracker {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            current_scan: 0,
        }
    }
}

impl AdjTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Marca início de um scan. Não reseta o mapa — `observe` decide continuidade.
    pub fn begin_scan(&mut self, scan: u64) {
        self.current_scan = scan;
    }

    /// Registra um ciclo `adj` visto neste scan. Retorna `seen_consecutive` atual.
    pub fn observe(&mut self, cycle_key: &str, scan: u64) -> usize {
        let prev_scan = scan.wrapping_sub(1);
        let seen_consecutive = match self.map.get(cycle_key) {
            Some(s) if s.last_seen_scan == prev_scan => s.seen_consecutive + 1,
            _ => 1,
        };
        let first_seen_scan = self
            .map
            .get(cycle_key)
            .map(|s| s.first_seen_scan)
            .unwrap_or(scan);
        self.map.insert(
            cycle_key.to_string(),
            AdjSeen {
                first_seen_scan,
                last_seen_scan: scan,
                seen_consecutive,
            },
        );
        seen_consecutive
    }

    /// Fim do scan: descarta ciclos não vistos há mais de `grace` scans (bounding
    /// de memória; o streak deles já está quebrado pela lógica de `observe`).
    pub fn end_scan(&mut self, scan: u64) {
        const GRACE: u64 = 10;
        self.map
            .retain(|_, s| scan.saturating_sub(s.last_seen_scan) <= GRACE);
    }

    /// Snapshot p/ diagnóstico/teste.
    pub fn snapshot(&self) -> &HashMap<String, AdjSeen> {
        &self.map
    }

    pub fn current_scan(&self) -> u64 {
        self.current_scan
    }
}

/// Hash curto e determinístico (4 hex chars) do `cycle_key` p/ o log `[ADJ] key=`.
/// FNV-1a 64-bit → low 16 bits. Determinístico: mesmas pernas+venues → mesmo hash.
/// Não é criptográfico — só identificador compacto/auditável (mesmas pernas em
/// ordem diferente dão hash diferente, o que é correto: ciclo distinto).
pub fn adj_key_hash(cycle_key: &str) -> String {
    const FNV_OFFSET: u64 = 0xcbf29ce484222325;
    const FNV_PRIME: u64 = 0x100000001b3;
    let mut h: u64 = FNV_OFFSET;
    for b in cycle_key.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    format!("{:04x}", (h & 0xffff) as u32)
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
pub fn extract_edges(pr: &HashMap<String, HashMap<String, f64>>) -> (usize, Vec<EdgeInfo>, CycleEconomics, Vec<AdjCycleInfo>) {
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
    let mut adj_cycles: Vec<AdjCycleInfo> = Vec::new();
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

            // INSTRUMENTAÇÃO `adj`: coleta composição + executabilidade p/ log
            // estruturado (persistência fica no caller, que tem estado entre scans).
            let (token_a, token_b) = pair.split_once('-').unwrap_or((pair.as_str(), ""));
            let legs = vec![
                AdjLeg {
                    venue: best_buy_dex.clone(),
                    token_in: token_a.to_string(),
                    token_out: token_b.to_string(),
                    rate: best_buy_price,
                },
                AdjLeg {
                    venue: best_sell_dex.clone(),
                    token_in: token_b.to_string(),
                    token_out: token_a.to_string(),
                    rate: best_sell_price,
                },
            ];
            let has_curve_leg = legs
                .iter()
                .any(|l| venue_curve_model(&l.venue) == CurveModel::StableSwap);
            let executable =
                route_all_legs_executable(legs.iter().map(|l| l.venue.as_str()));
            let cycle_key = legs
                .iter()
                .map(|l| format!("{}|{}>{}", l.venue, l.token_in, l.token_out))
                .collect::<Vec<_>>()
                .join("||");
            adj_cycles.push(AdjCycleInfo {
                cycle_key,
                pair: pair.clone(),
                legs,
                cycle_rate: best_cycle_rate,
                gross_profit_pct: (best_cycle_rate - 1.0) * 100.0,
                executable,
                has_curve_leg,
            });
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

    (count, edges, economics, adj_cycles)
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
    adj_tracker: &mut AdjTracker,
    cb: Arc<DexCircuitBreaker>,
    retry: &SmartRetryManager,
    cycle: u64,
) -> Result<(usize, Vec<EdgeInfo>)> {

    let (pairs, qf_enabled, min_spread, notional_usd, flashloan_fee_pct, gas_usd_est) = {
        let cfg = cfg.lock().await;
        (
            generate_full_pair_list(&cfg),
            cfg.optimization.quick_filter_enabled,
            cfg.optimization.min_spread_percent,
            cfg.arbitrage
                .default_trade_amount
                .parse::<f64>()
                .unwrap_or(100.0),
            // Custo (estimativa config-driven, honesta): premium Aave V3 verificado
            // on-chain (5 bps) + gas base. O `adj` do radar == gross fee-inclusive;
            // cost/net aqui é PROJEÇÃO downstream (economics/flashloan), não decisão.
            cfg.flashloan.fee_pct.unwrap_or(0.0005),
            cfg.execution.estimate_base_gas_usd,
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
    let (_signals, edges, economics, adj_cycles) = extract_edges(&out);

    log_price_audit(&out, cycle);
    log_edge_summary(&edges, cycle);
    log_edge_audit(&edges, cycle);

    // INSTRUMENTAÇÃO `adj`: log estruturado por ciclo que passou o ajuste de custo.
    // Persistência: adj_tracker mantém {cycle_key -> AdjSeen} entre scans; `persist`
    // = seen_consecutive (por quantos scans seguidos o mesmo ciclo apareceu como adj).
    // Ciclo estável = persist alto; intermitente = persist 1-2.
    //
    // Custo/net aqui é PROJEÇÃO config-driven (premium Aave 5 bps + gas base), não
    // decisão de execução — `adj` no radar == gross fee-inclusive (quotes já embutem
    // fee AMM). Flag `executable` separa "caixa" (route_all_legs_executable) de
    // "vitrine" (perna Curve, sem DexType on-chain hoje).
    adj_tracker.begin_scan(cycle);
    let mut adj_executable = 0usize;
    let mut adj_vitrine = 0usize;
    for ac in &adj_cycles {
        let gross_profit_usd = ac.gross_profit_pct / 100.0 * notional_usd;
        let flashloan_fee_usd = notional_usd * flashloan_fee_pct;
        let cost_usd = flashloan_fee_usd + gas_usd_est;
        let net_profit_usd = gross_profit_usd - cost_usd;
        let seen = adj_tracker.observe(&ac.cycle_key, cycle);
        let key = adj_key_hash(&ac.cycle_key);
        let legs_str = ac
            .legs
            .iter()
            .map(|l| format!("{}>{}>{}", l.token_in, l.venue, l.token_out))
            .collect::<Vec<_>>()
            .join(" | ");
        if ac.executable {
            adj_executable += 1;
        }
        if ac.has_curve_leg {
            adj_vitrine += 1;
        }
        info!(
            target: "adjcycle",
            "[ADJ] scan={} key={} legs={} cycle_rate={:.6} gross=${:.4} cost=${:.4} net=${:.4} \
             executable={} has_curve_leg={} persist={}",
            cycle,
            key,
            legs_str,
            ac.cycle_rate,
            gross_profit_usd,
            cost_usd,
            net_profit_usd,
            ac.executable,
            ac.has_curve_leg,
            seen,
        );
    }
    adj_tracker.end_scan(cycle);

    // Resumo por scan: adj_total = ciclos que passaram o ajuste (== gross_positive).
    // executable = caixa acionável (route_all_legs_executable); vitrine = tem perna Curve.
    if !adj_cycles.is_empty() {
        info!(
            target: "adjcycle",
            "[ADJ-SUMMARY] scan={} adj_total={} executable={} vitrine={}",
            cycle,
            adj_cycles.len(),
            adj_executable,
            adj_vitrine,
        );
    }

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
    let mut adj_tracker = AdjTracker::new();
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
                    &mut previous, &mut adj_tracker, cb.clone(),
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

    /// INSTRUMENTAÇÃO adj: ciclo com perna Curve é VITRINE (não executável).
    /// has_curve_leg=true, executable=false (route_all_legs_executable false
    /// por curve_executor_supported() -> false). Ciclo só QuickSwap/SushiSwap
    /// → executable=true, has_curve_leg=false (caixa).
    #[test]
    fn extract_adj_cycles_marca_vitrine_e_caxa() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // Ciclo VITRINE: cross Curve×QuickSwap no par USDC-USDT. extract_edges exige
        // ≥2 DEXes no par forward + 1 DEX distinto no reverse. Ambas DEXes cotam as
        // duas direções; o cross vencedor é buy=Curve(USDC>USDT) × sell=QuickSwap(USDT>USDC).
        // cycle_rate = 1.0001 * 1.0001 = 1.0002 > 1.0 → adj, has_curve_leg=true.
        pr.insert(
            "Curve".into(),
            m(&[("USDC-USDT", 1.0001), ("USDT-USDC", 1.0)]),
        );
        // QuickSwap cota os dois ciclos: vitrine (USDC-USDT) + caixa (AAA-BBB).
        // Tudo num insert só — segundo insert sobrescreveria o primeiro.
        pr.insert(
            "QuickSwap".into(),
            m(&[
                ("USDC-USDT", 1.0),
                ("USDT-USDC", 1.0001),
                ("AAA-BBB", 1.01),
                ("BBB-AAA", 1.0),
            ]),
        );
        // Ciclo CAIXA: cross QuickSwap×SushiSwap no par AAA-BBB.
        // buy=QuickSwap(AAA>BBB 1.01) × sell=SushiSwap(BBB>AAA 1.0) = 1.01 → adj,
        // has_curve_leg=false, executable=true.
        pr.insert(
            "SushiSwap".into(),
            m(&[("AAA-BBB", 1.0), ("BBB-AAA", 1.0)]),
        );

        let (_n, _edges, econ, adj) = extract_edges(&pr);

        let vitrine = adj.iter().find(|a| a.pair == "USDC-USDT");
        assert!(vitrine.is_some(), "deve haver adj USDC-USDT");
        let v = vitrine.unwrap();
        assert!(v.has_curve_leg, "USDC-USDT tem perna Curve");
        assert!(!v.executable, "Curve não é executável on-chain");
        assert!(v.cycle_rate > 1.0);
        // cycle_key determinístico e contém as duas pernas com venues.
        assert!(v.cycle_key.contains("Curve|USDC>USDT"));
        assert!(v.cycle_key.contains("QuickSwap|USDT>USDC"));

        let caxa = adj.iter().find(|a| a.pair == "AAA-BBB");
        assert!(caxa.is_some(), "deve haver adj AAA-BBB");
        let c = caxa.unwrap();
        assert!(!c.has_curve_leg);
        assert!(c.executable, "QuickSwap+SushiSwap é executável");
        assert!((c.cycle_rate - 1.01).abs() < 1e-9);

        // Contagem: ambos passaram o ajuste (gross == adj neste layer).
        assert!(econ.venue_fee_adjusted_positive >= 2);
    }

    /// Persistência: AdjTracker conta consecutivos e quebra após gap.
    #[test]
    fn adj_tracker_conta_consecutivos_e_quebra_apos_gap() {
        let mut t = AdjTracker::new();
        t.begin_scan(1);
        assert_eq!(t.observe("k", 1), 1); // primeira aparição
        t.end_scan(1);

        t.begin_scan(2);
        assert_eq!(t.observe("k", 2), 2); // consecutivo
        t.end_scan(2);

        // scan 3: gap — "k" não aparece.
        t.begin_scan(3);
        t.end_scan(3);

        t.begin_scan(4);
        assert_eq!(t.observe("k", 4), 1, "gap em scan 3 deve zerar o streak");
        t.end_scan(4);

        let snap = t.snapshot();
        assert_eq!(snap.get("k").unwrap().seen_consecutive, 1);
        assert_eq!(snap.get("k").unwrap().first_seen_scan, 1); // preserva first_seen
        assert_eq!(snap.get("k").unwrap().last_seen_scan, 4);
    }

    /// (b) cycle_key + adj_key_hash são determinísticos: mesmas pernas+venues em
    /// ordem igual → mesma chave e mesmo hash curto. Orem diferente → hash distinto
    /// (ciclo distinto). Persistência (`persist`) depende desta estabilidade.
    #[test]
    fn adj_cycle_key_e_hash_sao_deterministicos() {
        // Mesmo ciclo (mesmas pernas+venues, mesma ordem) em dois HashMaps de input
        // com ordem de iteração distinta → mesmo cycle_key → mesmo hash. Ambas DEXes
        // cotam as duas direções (extract_edges exige ≥2 DEXes no par forward).
        let mut pr_a: HashMap<String, HashMap<String, f64>> = HashMap::new();
        pr_a.insert("Curve".into(), m(&[("USDC-USDT", 1.0001), ("USDT-USDC", 1.0)]));
        pr_a.insert("QuickSwap".into(), m(&[("USDC-USDT", 1.0), ("USDT-USDC", 1.0001)]));

        let mut pr_b: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // Inserção em ordem trocada — iteração pode variar, mas o ciclo é o mesmo.
        pr_b.insert("QuickSwap".into(), m(&[("USDC-USDT", 1.0), ("USDT-USDC", 1.0001)]));
        pr_b.insert("Curve".into(), m(&[("USDC-USDT", 1.0001), ("USDT-USDC", 1.0)]));

        let (_, _, _, adj_a) = extract_edges(&pr_a);
        let (_, _, _, adj_b) = extract_edges(&pr_b);

        let ka = adj_a.iter().find(|a| a.pair == "USDC-USDT").unwrap();
        let kb = adj_b.iter().find(|a| a.pair == "USDC-USDT").unwrap();
        assert_eq!(ka.cycle_key, kb.cycle_key, "cycle_key deve ser igual p/ mesmo ciclo");
        assert_eq!(adj_key_hash(&ka.cycle_key), adj_key_hash(&kb.cycle_key));

        // Hash determinístico p/ string fixa (snapshot estável entre runs).
        assert_eq!(adj_key_hash("Curve|USDC>USDT||QuickSwap|USDT>USDC"),
                   adj_key_hash("Curve|USDC>USDT||QuickSwap|USDT>USDC"));
        // 4 hex chars.
        let h = adj_key_hash("Curve|USDC>USDT||QuickSwap|USDT>USDC");
        assert_eq!(h.len(), 4);
        assert!(h.chars().all(|c| c.is_ascii_hexdigit()));
        // Ordem de pernas diferente → cycle_key diferente → hash diferente.
        assert_ne!(adj_key_hash("A|X>Y||B|Y>X"), adj_key_hash("B|Y>X||A|X>Y"));
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

        let (_n, edges, econ, _adj) = extract_edges(&pr);
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

        let (_n, edges, _, _adj) = extract_edges(&pr);
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
