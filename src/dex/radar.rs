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
        cached_fee_tier,
        liquidity::min_pool_liquidity_usd_for_dex,
        manager::DexManager,
        rate_limiter::{ALCHEMY_RATE_LIMITER, DEX_RATE_LIMITER},
    },
};
use chrono::Utc;
use ethers::providers::{Middleware, Provider, Ws};
use futures_util::StreamExt;
use anyhow::{anyhow, Result};
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

        // M19: mediana dos outros DEX. Média deixa um pool raso/outlier puxar
        // referência e pode pular uma divergência saudável ou cotar ruído.
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

        let reference = median(&others);
        if reference <= 0.0 || !reference.is_finite() || !prev_price.is_finite() || prev_price <= 0.0 {
            return true;
        }
        let spread = ((reference - prev_price).abs() / prev_price) * 100.0;

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
    quote_block: ethers::types::U64,
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
            Box::pin(async move {
                dm2.get_prices_multicall(&ad, &batch, Some(quote_block))
                    .await
            })
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
            "liquidity gate: pares cortados neste scan por TVL proxy < threshold"
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

/// Parâmetros de custo config-driven p/ projetar net de um ciclo `adj`.
/// `adj` no radar == gross fee-inclusive (quotes já embutem fee AMM); o desconto
/// de gas + flashloan fee é downstream (economics/flashloan). Estes valores são a
/// PROJEÇÃO usada só p/ log/TUI — não são decisão de execução.
///
/// - `flashloan_fee_pct`: premium Aave V3 verificado on-chain (5 bps = 0.0005).
/// - `gas_usd_est`: gas base estimado em USD (config.execution.estimate_base_gas_usd).
#[derive(Clone, Copy, Debug)]
pub struct AdjCostParams {
    pub notional_usd: f64,
    pub flashloan_fee_pct: f64,
    pub gas_usd_est: f64,
}

impl AdjCostParams {
    /// Net projetado (USD) de um ciclo dado seu `gross_profit_pct` (= (cycle_rate-1)*100).
    /// cost = flashloan_fee + gas; net = gross - cost.
    pub fn net_usd(&self, gross_profit_pct: f64) -> f64 {
        let gross = gross_profit_pct / 100.0 * self.notional_usd;
        let cost = self.notional_usd * self.flashloan_fee_pct + self.gas_usd_est;
        gross - cost
    }

    /// Custo total projetado (USD) = flashloan_fee + gas.
    pub fn cost_usd(&self) -> f64 {
        self.notional_usd * self.flashloan_fee_pct + self.gas_usd_est
    }
}

impl Default for AdjCostParams {
    fn default() -> Self {
        // Defaults alinhados ao config (Aave 5 bps, gas base $0.008, notional $100).
        Self { notional_usd: 100.0, flashloan_fee_pct: 0.0005, gas_usd_est: 0.008 }
    }
}

/// Chave canônica de um round-trip, INDEPENDENTE da direção do par (A-B vs B-A).
/// Espelhos (mesmas pernas invertidas) colidem nesta chave p/ dedup. Composta por:
///   venues sorted ∪ token-pair sorted
/// Ex.: USDC-USDT em Curve×UniswapV3 → "Curve|UniswapV3|USDC|USDT" (mesmo valor
/// p/ USDT-USDC espelhado). venues distintas ou token-pair distinto → chave distinta
/// (ciclo economicamente distinto, NÃO dedup).
fn adj_canonical_key(legs: &[AdjLeg]) -> String {
    let mut venues: Vec<&str> = legs.iter().map(|l| l.venue.as_str()).collect();
    venues.sort();
    venues.dedup();
    let mut tokens: Vec<&str> = legs
        .iter()
        .flat_map(|l| [l.token_in.as_str(), l.token_out.as_str()])
        .collect();
    tokens.sort();
    tokens.dedup();
    format!("{}|{}", venues.join("|"), tokens.join("|"))
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
    /// Net projetado (USD) = gross - (flashloan_fee + gas). PROJEÇÃO config-driven,
    /// não decisão de execução. Negativo → custo > lucro → oportunidade ilusória.
    pub net_profit_usd: f64,
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
    /// Ciclos `adj` deduped (mirror A-B/B-A = 1) com net projetado > 0
    /// (gross > flashloan_fee + gas). Reflete realidade pós-custo, não só gross.
    pub net_positive: usize,
}

/// Melhor 2-hop cross-DEX: buy forward (venue A) × sell reverse (venue B), A≠B,
/// maximizando `cycle_rate = buy_price × sell_price`. Quotes já fee+impact-inclusive
/// (getAmountsOut/Quoter/get_dy) — NÃO reaplicar fee. Retorna `None` se não há
/// combinação buy≠sell com cycle_rate > 0 (ex.: sem reverse, ou só 1 venue).
///
/// Compartilhado entre `extract_edges` (que só conta cycle_rate > 1.0) e
/// `analyze_pair_spread` (top-N, que revela também os ≤1.0).
fn best_two_hop(
    forward: &[(String, f64)],
    reverse: &[(String, f64)],
) -> Option<(String, f64, String, f64, f64)> {
    let mut best_rate: f64 = 0.0;
    let mut best: Option<(String, f64, String, f64)> = None;
    for (buy_venue, buy_price) in forward {
        for (sell_venue, sell_price) in reverse {
            if buy_venue == sell_venue {
                continue;
            }
            let rate = buy_price * sell_price;
            if rate > best_rate {
                best_rate = rate;
                best = Some((buy_venue.clone(), *buy_price, sell_venue.clone(), *sell_price));
            }
        }
    }
    best.map(|(bv, bp, sv, sp)| (bv, bp, sv, sp, best_rate))
}

/// Conta sinais de spread E extrai os edges (> 0.01%) para logging/audit.
///
/// **VALIDAÇÃO CROSS-DEX**: Só emite EDGE quando `cycle_rate = buy_price × sell_price`
/// (quotes já fee-inclusive via getAmountsOut / Quoter) é > 1.0.
/// Spreads single-direction (ex: USDT-WMATIC 2.42% sem reverso viável) são filtrados.
pub fn extract_edges(
    pr: &HashMap<String, HashMap<String, f64>>,
    cost: &AdjCostParams,
) -> (usize, Vec<EdgeInfo>, CycleEconomics, Vec<AdjCycleInfo>) {
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
    // M12: negativos também representam round-trips, portanto A-B/B-A devem
    // contar uma vez, igual a `adj_cycles`. A chave inclui tokens e venues para
    // não colapsar ciclos distintos no mesmo par.
    let mut negative_cycle_keys: std::collections::HashSet<String> =
        std::collections::HashSet::new();

    for (pair, dex_prices) in &map_pairs {
        if dex_prices.len() < 2 {
            continue;
        }

        // Extrair reverse pair (ex: USDT-WMATIC → WMATIC-USDT)
        let reverse_pair = pair.split_once('-').map(|(a, b)| format!("{}-{}", b, a));

        // Buscar preços do reverse pair se existir
        let reverse_dex_prices = reverse_pair.as_ref().and_then(|rp| map_pairs.get(rp));

        // Melhor 2-hop cross-DEX (buy forward × sell reverse, A≠B), max cycle_rate.
        let best = match reverse_dex_prices {
            Some(rev_prices) => best_two_hop(dex_prices, rev_prices),
            None => None,
        };

        let Some((best_buy_dex, best_buy_price, best_sell_dex, best_sell_price, best_cycle_rate)) = best
        else {
            // Sem reverso disponível — não há como avaliar ciclo
            continue;
        };

        evaluated += 1;

        // Contabilizar economia do ciclo (gross == fee-inclusive product)
        if best_cycle_rate > 1.0 {
            let gross_profit_pct = (best_cycle_rate - 1.0) * 100.0;
            // A9: guard MAX_SPREAD_PCT ANTES do push em adj_cycles. Antes o push
            // acontecia primeiro (linha ~744) e só depois `if spread > MAX continue`
            // — ciclos dust-pool com spread > 50% entravam em adj_total /
            // gross_positive / net_positive no [ADJ-SUMMARY], inflando oportunidade
            // real. Agora descarta antes de qualquer contagem.
            if gross_profit_pct > MAX_SPREAD_PCT {
                debug!(
                    "🚫 [DUST] {} spread={:.2}% > {:.0}% — pool raso, descartado antes do adj_cycles.push",
                    pair, gross_profit_pct, MAX_SPREAD_PCT
                );
                continue;
            }
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
                gross_profit_pct,
                net_profit_usd: cost.net_usd(gross_profit_pct),
                executable,
                has_curve_leg,
            });
        } else {
            let (token_a, token_b) = pair.split_once('-').unwrap_or((pair.as_str(), ""));
            let negative_legs = [
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
            negative_cycle_keys.insert(adj_canonical_key(&negative_legs));
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

        // A8: dedup mirror em edges/count. `extract_edges` itera ambos os sentidos
        // do par (generate_full_pair_list é direcional), e A-B / B-A produzem o
        // MESMO round-trip econômico (cycle_rate idêntico). Sem este filtro, cada
        // ciclo vira 2 EdgeInfo no log/CSV `[EDGE]` e 2 sinais em `count` —
        // double-count direto. Mantém só a direção canônica (pair < reverse),
        // empatando estável-true via `canonical_pair_name` p/ label consistente
        // com `adj_cycles` (que dedupa por `adj_canonical_key` em separado).
        let is_canonical_dir = match &reverse_pair {
            Some(rp) => pair.as_str() < rp.as_str(),
            None => true,
        };
        if !is_canonical_dir {
            continue;
        }
        if spread_pct >= 0.03 {
            count += 1;
        }
        if spread_pct >= 0.01 {
            let (ta, tb) = pair.split_once('-').unwrap_or((pair.as_str(), ""));
            edges.push(EdgeInfo {
                pair: canonical_pair_name(ta, tb),
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

    // DEDUP mirror pairs: A-B e B-A produzem o mesmo round-trip econômico (pernas
    // invertidas, mesmo cycle_rate). `extract_edges` itera ambos os sentidos do par
    // (generate_full_pair_list é direcional), então sem dedup o mesmo ciclo vira 2
    // adj. Colapsa por `adj_canonical_key` (venues+tokens sorted, direção-agnóstica),
    // mantendo a entrada de MAIOR net projetado.
    //
    // Tiebreak determinístico: espelhos simétricos empatam em net → desempata por
    // cycle_key ASC (menor string primeiro). Assim o sobrevivente é o mesmo qualquer
    // que seja a ordem de iteração do HashMap de input (estável entre scans/runs).
    // O `pair` do sobrevivente é normalizado p/ forma canônica (direção-agnóstica),
    // já que o round-trip espelhado não tem direção preferida.
    adj_cycles.sort_by(|a, b| {
        b.net_profit_usd
            .partial_cmp(&a.net_profit_usd)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cycle_key.cmp(&b.cycle_key))
    });
    let mut deduped: Vec<AdjCycleInfo> = Vec::with_capacity(adj_cycles.len());
    let mut seen_canon: std::collections::HashSet<String> = std::collections::HashSet::new();
    for mut ac in adj_cycles {
        let canon = adj_canonical_key(&ac.legs);
        if seen_canon.insert(canon) {
            // Normaliza o label do par p/ forma canônica (stable-first / sorted),
            // independente de qual direção (A-B vs B-A) ganhou o desempate.
            if let Some((a, b)) = ac.pair.split_once('-') {
                ac.pair = canonical_pair_name(a, b);
            }
            deduped.push(ac);
        }
    }
    let net_positive = deduped.iter().filter(|a| a.net_profit_usd > 0.0).count();
    // gross/adj e negativos contam ciclos ÚNICOS pós-dedup (mirror A-B/B-A = 1),
    // consistente com adj_total e net_positive. `evaluated` permanece por direção
    // (cobertura do scan, não oportunidades).
    let gross_dedup = deduped.len();

    let economics = CycleEconomics {
        evaluated,
        gross_positive: gross_dedup,
        venue_fee_adjusted_positive: gross_dedup,
        negative_cycles_found: negative_cycle_keys.len(),
        net_positive,
    };

    (count, edges, economics, deduped)
}

// ============================================================
// TOP-N SPREADS: cycle_rate real bidirecional + TVL por pool
// ============================================================
// Revela o que a coluna Spread% do TUI esconde: a perna REVERSA (com fee+impact
// próprios, não 1/rate_forward) e a profundidade de cada pool. Spread alto que
// não fecha = cycle_rate real ≤1 OU pool raso destoando (TVL baixo, outlier).

pub struct TopSpreadLeg {
    pub venue: String,
    pub token_in: String,
    pub token_out: String,
    pub rate: f64,
}

pub struct TopSpreadInfo {
    pub pair: String,
    /// Spread% single-dir do TUI: (max-min)/min*100 das cotações forward.
    pub tui_spread_pct: f64,
    pub leg1: Option<TopSpreadLeg>, // forward (buy), None se sem reverse
    pub leg2: Option<TopSpreadLeg>, // reverse (sell), None se sem reverse
    pub cycle_rate: Option<f64>,    // None se não há 2-hop buy≠sell
    pub gross_pct: Option<f64>,
    pub net_usd: Option<f64>,
    pub outlier: Option<String>, // venue que destoa da mediana forward (suspeito raso)
    pub executable: bool,
    pub has_curve_leg: bool,
}

/// Spread% single-dir idêntico à coluna do TUI (`tui.rs:221-224`).
fn tui_spread_pct(forward: &[(String, f64)]) -> f64 {
    if forward.len() < 2 {
        return 0.0;
    }
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for (_, p) in forward {
        if *p <= 0.0 || !p.is_finite() {
            continue;
        }
        if *p < min {
            min = *p;
        }
        if *p > max {
            max = *p;
        }
    }
    if min.is_finite() && min > 0.0 {
        (max - min) / min * 100.0
    } else {
        0.0
    }
}

/// Mediana dos preços forward (p/ outlier). 50º percentil da amostra ordenada.
fn median(values: &[f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut v: Vec<f64> = values.iter().copied().filter(|x| x.is_finite() && *x > 0.0).collect();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if v.is_empty() {
        return 0.0;
    }
    let mid = v.len() / 2;
    if v.len() % 2 == 0 {
        (v[mid - 1] + v[mid]) / 2.0
    } else {
        v[mid]
    }
}

/// Venue cujo preço forward mais se afasta da mediana — o suspeito de pool raso
/// que cria o spread aparente. None se <2 venues válidas.
fn outlier_venue(forward: &[(String, f64)]) -> Option<String> {
    let prices: Vec<f64> = forward.iter().map(|(_, p)| *p).collect();
    let med = median(&prices);
    if med <= 0.0 {
        return None;
    }
    forward
        .iter()
        .filter(|(_, p)| p.is_finite() && *p > 0.0)
        .max_by(|(_, a), (_, b)| {
            (a - med).abs().partial_cmp(&(b - med).abs()).unwrap_or(std::cmp::Ordering::Equal)
        })
        .map(|(v, _)| v.clone())
}

/// Core puro (sem RPC) — testável. `reverse` vazio → sem perna reversa no scan.
pub fn analyze_pair_spread(
    pair: &str,
    forward: &[(String, f64)],
    reverse: &[(String, f64)],
    cost: &AdjCostParams,
) -> TopSpreadInfo {
    let tui_spread_pct = tui_spread_pct(forward);
    let best = best_two_hop(forward, reverse);
    let (leg1, leg2, cycle_rate, gross_pct, net_usd, executable, has_curve_leg) = match best {
        Some((bv, bp, sv, sp, rate)) => {
            let (a, b) = pair.split_once('-').unwrap_or((pair, ""));
            let leg1 = TopSpreadLeg {
                venue: bv.clone(),
                token_in: a.to_string(),
                token_out: b.to_string(),
                rate: bp,
            };
            let leg2 = TopSpreadLeg {
                venue: sv.clone(),
                token_in: b.to_string(),
                token_out: a.to_string(),
                rate: sp,
            };
            let gross = (rate - 1.0) * 100.0;
            let net = cost.net_usd(gross);
            let venues = [leg1.venue.as_str(), leg2.venue.as_str()];
            let executable = route_all_legs_executable(venues.into_iter());
            let has_curve = venues
                .iter()
                .any(|v| venue_curve_model(v) == CurveModel::StableSwap);
            (Some(leg1), Some(leg2), Some(rate), Some(gross), Some(net), executable, has_curve)
        }
        None => (None, None, None, None, None, false, false),
    };
    TopSpreadInfo {
        pair: pair.to_string(),
        tui_spread_pct,
        leg1,
        leg2,
        cycle_rate,
        gross_pct,
        net_usd,
        outlier: outlier_venue(forward),
        executable,
        has_curve_leg,
    }
}

/// TOP-N spreads (por Spread% do TUI, desc) — **sync, sem TVL/RPC**. Espelha o
/// ranking do log `[TOPSPREAD]` mas sem ler profundidade (TVL fica só no log).
/// Usado pelo painel do TUI. `n==0` → vazio.
pub fn compute_top_spreads(
    prices: &HashMap<String, HashMap<String, f64>>,
    cost: &AdjCostParams,
    n: usize,
) -> Vec<TopSpreadInfo> {
    if n == 0 || prices.is_empty() {
        return Vec::new();
    }

    let mut forward_by_pair: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for (dex, map) in prices {
        for (pair, price) in map {
            forward_by_pair
                .entry(pair.clone())
                .or_default()
                .push((dex.clone(), *price));
        }
    }

    // M3: ranking por net_usd (ou cycle_rate), não por tui_spread_pct (single-dir).
    // Antes pares com spread nominal alto mas cycle_rate ≤ 1 (sem arb real)
    // ficavam acima de pares com net positivo. Agora computa analyze_pair_spread
    // (cycle_rate = buy×sell, fee-inclusive) e ordena por net desc; tui_spread
    // permanece só como coluna informativa em TopSpreadInfo.
    let mut ranked: Vec<TopSpreadInfo> = forward_by_pair
        .iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(p, v)| {
            let fwd = v.clone();
            let reverse = p
                .split_once('-')
                .map(|(a, b)| format!("{}-{}", b, a));
            let rev: Vec<(String, f64)> = reverse
                .as_ref()
                .and_then(|r| forward_by_pair.get(r))
                .cloned()
                .unwrap_or_default();
            analyze_pair_spread(p, &fwd, &rev, cost)
        })
        .filter(|info| info.cycle_rate.is_some())
        .collect();
    ranked.sort_by(|a, b| {
        // net_usd desc; sem net → −∞. Tiebreak: cycle_rate desc.
        let na = a.net_usd.unwrap_or(f64::NEG_INFINITY);
        let nb = b.net_usd.unwrap_or(f64::NEG_INFINITY);
        nb.partial_cmp(&na)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ra = a.cycle_rate.unwrap_or(0.0);
                let rb = b.cycle_rate.unwrap_or(0.0);
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    ranked.into_iter().take(n).collect()
}

/// Formata TVL USD compacto: $1.2M / $340k / $123. None/inválido → "tvl=?".
fn fmt_tvl(tvl: Option<f64>) -> String {
    match tvl {
        Some(v) if v.is_finite() && v > 0.0 => {
            if v >= 1e6 {
                format!("tvl=${:.1}M", v / 1e6)
            } else if v >= 1e3 {
                format!("tvl=${:.0}k", v / 1e3)
            } else {
                format!("tvl=${:.0}", v)
            }
        }
        _ => "tvl=?".to_string(),
    }
}

/// Log dos TOP-N spreads (por Spread% do TUI, desc). Read-only: TVL via
/// `DexManager::pool_tvl_usd` (balanceOf multicall, sem gas). Revela cycle_rate
/// real bidirecional + profundidade de cada pool do melhor 2-hop daquele par.
async fn log_top_n_spreads(
    out: &HashMap<String, HashMap<String, f64>>,
    dm: &Arc<DexManager>,
    adj_cost: &AdjCostParams,
    n: usize,
    cycle: u64,
) {
    if n == 0 || out.is_empty() {
        return;
    }

    // forward_by_pair: cada par (direcional) -> [(venue, price)]
    let mut forward_by_pair: HashMap<String, Vec<(String, f64)>> = HashMap::new();
    for (dex, map) in out {
        for (pair, price) in map {
            forward_by_pair
                .entry(pair.clone())
                .or_default()
                .push((dex.clone(), *price));
        }
    }

    // M3: igual ao caminho sync (`compute_top_spreads`), ranqueia por net_usd,
    // não pelo spread single-dir do TUI. Assim log assíncrono não prioriza
    // spread nominal alto sem lucro líquido projetado.
    let mut ranked: Vec<TopSpreadInfo> = forward_by_pair
        .iter()
        .filter(|(_, v)| v.len() >= 2)
        .map(|(p, v)| {
            let fwd = v.clone();
            let reverse = p
                .split_once('-')
                .map(|(a, b)| format!("{}-{}", b, a));
            let rev: Vec<(String, f64)> = reverse
                .as_ref()
                .and_then(|r| forward_by_pair.get(r))
                .cloned()
                .unwrap_or_default();
            analyze_pair_spread(p, &fwd, &rev, adj_cost)
        })
        .filter(|info| info.cycle_rate.is_some())
        .collect();
    ranked.sort_by(|a, b| {
        let na = a.net_usd.unwrap_or(f64::NEG_INFINITY);
        let nb = b.net_usd.unwrap_or(f64::NEG_INFINITY);
        nb.partial_cmp(&na)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                let ra = a.cycle_rate.unwrap_or(0.0);
                let rb = b.cycle_rate.unwrap_or(0.0);
                rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
            })
    });

    let cfg = dm.config_ref();
    // A10: fee_hint por perna. Antes `unwrap_or(0)` quando venue sem `fee_tier` no
    // config — p/ V3 (tiers 100/500/3000/10000) fee=0 resolve pool inexistente ou
    // diferente do cotado → TVL do pool errado → shallow-flag e diagnóstico falsos.
    // Agora: fee_tier real do adapter (cache populado pelo multicall best-fee do
    // V3) → fallback config.fee_tier → None (fail-open, TVL não lida) em vez de 0.
    let fee_for = |venue: &str, token_in: &str, token_out: &str| -> Option<u32> {
        cached_fee_tier(venue, token_in, token_out)
            .or_else(|| {
                cfg.dex
                    .iter()
                    .find(|d| d.name.eq_ignore_ascii_case(venue))
                    .and_then(|d| d.fee_tier)
            })
    };

    for info in ranked.into_iter().take(n) {
        let tui_spread = info.tui_spread_pct;

        // TVL read-only de cada perna do melhor 2-hop. None = fail-open (Curve, fee
        // desconhecido, etc.). Só lê quando fee_hint resolvido — fee=0 p/ V3 é pool
        // errado (ver A10).
        let (tvl1_str, shallow1) = match &info.leg1 {
            Some(l) => {
                let tvl = match fee_for(&l.venue, &l.token_in, &l.token_out) {
                    Some(fee) => dm
                        .pool_tvl_usd(&l.venue, &l.token_in, &l.token_out, fee)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                };
                let min = min_pool_liquidity_usd_for_dex(cfg, &l.venue);
                let shallow = matches!(tvl, Some(t) if t < min);
                (fmt_tvl(tvl), shallow)
            }
            None => ("tvl=?".to_string(), false),
        };
        let (tvl2_str, shallow2) = match &info.leg2 {
            Some(l) => {
                let tvl = match fee_for(&l.venue, &l.token_in, &l.token_out) {
                    Some(fee) => dm
                        .pool_tvl_usd(&l.venue, &l.token_in, &l.token_out, fee)
                        .await
                        .ok()
                        .flatten(),
                    None => None,
                };
                let min = min_pool_liquidity_usd_for_dex(cfg, &l.venue);
                let shallow = matches!(tvl, Some(t) if t < min);
                (fmt_tvl(tvl), shallow)
            }
            None => ("tvl=?".to_string(), false),
        };

        let leg1_str = match &info.leg1 {
            Some(l) => format!(
                "leg1={}:{}→{}@{:.6}({}{})",
                l.venue, l.token_in, l.token_out, l.rate, tvl1_str,
                if shallow1 { " SHALLOW" } else { "" }
            ),
            None => "leg1=NONE".to_string(),
        };
        let leg2_str = match &info.leg2 {
            Some(l) => format!(
                "leg2={}:{}→{}@{:.6}({}{})",
                l.venue, l.token_in, l.token_out, l.rate, tvl2_str,
                if shallow2 { " SHALLOW" } else { "" }
            ),
            None => "leg2=NONE".to_string(),
        };

        // Sem reverse cotado → cycle_rate indisponível (razão do não-fechamento).
        if info.cycle_rate.is_none() {
            info!(
                target: "topspread",
                "[TOPSPREAD] scan={} {} tui_spread={:.2}% {} {} cycle_rate=N/A outlier={} reason=no-reverse",
                cycle, info.pair, tui_spread, leg1_str, leg2_str,
                info.outlier.as_deref().unwrap_or("?"),
            );
            continue;
        }

        info!(
            target: "topspread",
            "[TOPSPREAD] scan={} {} tui_spread={:.2}% {} {} cycle_rate={:.6} gross={:.2}% net=${:.4} exec={} curve={} outlier={}",
            cycle, info.pair, tui_spread, leg1_str, leg2_str,
            info.cycle_rate.unwrap_or(0.0),
            info.gross_pct.unwrap_or(0.0),
            info.net_usd.unwrap_or(0.0),
            info.executable,
            info.has_curve_leg,
            info.outlier.as_deref().unwrap_or("?"),
        );
    }
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
    adj_cost: &AdjCostParams,
    price_tx: &mpsc::Sender<HashMap<String, HashMap<String, f64>>>,
    previous: &mut HashMap<String, HashMap<String, f64>>,
    adj_tracker: &mut AdjTracker,
    cb: Arc<DexCircuitBreaker>,
    retry: &SmartRetryManager,
    cycle: u64,
) -> Result<(usize, Vec<EdgeInfo>)> {

    let (pairs, qf_enabled, min_spread, top_n) = {
        let cfg = cfg.lock().await;
        (
            generate_full_pair_list(&cfg),
            cfg.optimization.quick_filter_enabled,
            cfg.optimization.min_spread_percent,
            cfg.log.top_spreads_n,
        )
    };

    let adapters = get_healthy_adapters(dm, &cb).await;
    let qf = HighHitRateFilter::new(qf_enabled, min_spread);
    let quote_block = dm.quote_block_number().await?;
    debug!(block = %quote_block, "snapshot único para cotações cross-DEX");

    let mut tasks = task::JoinSet::new();

    for ad in adapters {
        let prev = previous.clone();
        let batch = pairs.clone();
        let dm2 = dm.clone();
        let cb2 = cb.clone();
        let retry2 = retry.clone();
        let qf2 = qf.clone();

        tasks.spawn(async move {
            collect_dex_prices(ad, batch, qf2, prev, dm2, cb2, retry2, quote_block).await
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
    let (_signals, edges, economics, adj_cycles) = extract_edges(&out, adj_cost);

    log_price_audit(&out, cycle);
    log_edge_summary(&edges, cycle);
    log_edge_audit(&edges, cycle);

    // TOP-N spreads: revela o que a coluna Spread% do TUI esconde (perna reversa
    // real + profundidade). Read-only (TVL via balanceOf), sem gas.
    log_top_n_spreads(&out, dm, adj_cost, top_n, cycle).await;

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
    let cost_usd = adj_cost.cost_usd();
    for ac in &adj_cycles {
        // net/cost já computados em extract_edges (fonte única, config-driven).
        let gross_profit_usd = ac.gross_profit_pct / 100.0 * adj_cost.notional_usd;
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
            ac.net_profit_usd,
            ac.executable,
            ac.has_curve_leg,
            seen,
        );
    }
    adj_tracker.end_scan(cycle);

    // Resumo por scan: adj_total = ciclos deduped (mirror A-B/B-A = 1, não 2).
    // executable = caixa acionável (route_all_legs_executable); vitrine = perna Curve.
    // net_pos = ciclos com net projetado > 0 (gross > flashloan_fee + gas) — realidade
    // pós-custo, não só gross. Mostra o valor líquido real da "oportunidade".
    if !adj_cycles.is_empty() {
        info!(
            target: "adjcycle",
            "[ADJ-SUMMARY] scan={} adj_total={} executable={} vitrine={} net_pos={}",
            cycle,
            adj_cycles.len(),
            adj_executable,
            adj_vitrine,
            economics.net_positive,
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
    adj_cost: Arc<AdjCostParams>,
    cb: Arc<DexCircuitBreaker>,
    price_tx: mpsc::Sender<HashMap<String, HashMap<String, f64>>>,
    mut shutdown_rx: broadcast::Receiver<()>
) -> Result<()> {

    let mut stream = ws.subscribe_blocks().await?;
    let retry = SmartRetryManager::new(2, Duration::from_millis(35)).with_jitter(0.2);
    let mut previous: HashMap<String, HashMap<String, f64>> = HashMap::new();
    let mut adj_tracker = AdjTracker::new();
    let mut cycle = 0u64;

    // WS morre silenciosamente: stream.next() dá None (fechou) ou trava
    // (half-open). Sem tratamento, o select! bloqueava pra sempre — o radar
    // nunca retornava Err, o auto-restart (main.rs) nunca disparava, e a TUI
    // congelava: heartbeat piscando mas scan_age crescendo, parecia viva mas
    // nunca atualizava. Agora: None ou ausência de bloco > BLOCK_TIMEOUT
    // → Err → auto-restart → connect_ws itera próximos endpoints.
    // Polygon ~2s/block; 20s sem bloco = WS morto/half-open.
    const BLOCK_TIMEOUT: Duration = Duration::from_secs(20);

    loop {
        tokio::select! {
            _ = shutdown_rx.recv() => {
                info!("📡 Radar encerrado após {} ciclos", cycle);
                break;
            }

            block = stream.next() => {
                match block {
                    Some(_) => {
                        cycle += 1;

                        ALCHEMY_RATE_LIMITER.cleanup().await;
                        DEX_RATE_LIMITER.cleanup().await;

                        match execute_radar_cycle(
                            &dm, &cfg, &adj_cost, &price_tx,
                            &mut previous, &mut adj_tracker, cb.clone(),
                            &retry, cycle
                        ).await {
                            Ok((total, edges)) =>
                                debug!("Ciclo {} — {} preços, {} sinais-spread, {} edges logados", cycle, total, edges.len(), edges.len()),
                            Err(e) =>
                                error!("Erro no ciclo {}: {:?}", cycle, e),
                        }
                    }
                    None => {
                        error!("📡 Radar: stream WS fechou (provider morto) — disparando failover.");
                        return Err(anyhow!("WS stream closed — failing over to next endpoint"));
                    }
                }
            }

            _ = tokio::time::sleep(BLOCK_TIMEOUT) => {
                error!("📡 Radar: sem bloco há {}s — WS morto/half-open, disparando failover.", BLOCK_TIMEOUT.as_secs());
                return Err(anyhow!("no block within {}s — WS dead, failing over", BLOCK_TIMEOUT.as_secs()));
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
    fn quick_filter_ignora_outlier_ao_calcular_referencia() {
        let mut previous = HashMap::new();
        previous.insert("QuickSwap".into(), m(&[("AAA-BBB", 0.99)]));
        previous.insert("SushiSwap".into(), m(&[("AAA-BBB", 1.00)]));
        previous.insert("UniswapV3".into(), m(&[("AAA-BBB", 1.00)]));
        previous.insert("PoolRaso".into(), m(&[("AAA-BBB", 100.0)]));

        let filter = HighHitRateFilter::new(true, 5.0);
        let cb = DexCircuitBreaker::new(3, 60);

        assert!(
            !filter.should_analyze_dex_pair("QuickSwap", "AAA-BBB", &previous, &cb),
            "mediana 1.0 mantém desvio de 0.99 abaixo do threshold; média seria puxada pelo outlier"
        );
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

        let (_n, _edges, econ, adj) = extract_edges(&pr, &AdjCostParams::default());

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

        let (_, _, _, adj_a) = extract_edges(&pr_a, &AdjCostParams::default());
        let (_, _, _, adj_b) = extract_edges(&pr_b, &AdjCostParams::default());

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

    /// Dedup mirror pairs: A-B e B-A (pernas invertidas) = MESMO round-trip econômico
    /// → colapsa em 1 adj, não 2. E net_positive conta só ciclos com net projetado > 0
    /// (gross > flashloan_fee + gas), refletindo realidade pós-custo.
    #[test]
    fn extract_adj_cycles_dedup_mirror_e_conta_net_positive() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // Ciclo 1 (vitrine, net-NEGATIVO): Curve×QuickSwap USDC-USDT cycle_rate 1.0002.
        //   gross = 0.02% × $100 = $0.02; cost = $0.058 → net = -$0.038.
        pr.insert(
            "Curve".into(),
            m(&[("USDC-USDT", 1.0001), ("USDT-USDC", 1.0)]),
        );
        // QuickSwap cota os dois ciclos — num insert só (segundo sobrescreveria).
        pr.insert(
            "QuickSwap".into(),
            m(&[
                ("USDC-USDT", 1.0),
                ("USDT-USDC", 1.0001),
                ("AAA-BBB", 1.01),
                ("BBB-AAA", 1.0),
            ]),
        );
        // Ciclo 2 (caixa, net-POSITIVO): QuickSwap×SushiSwap AAA-BBB cycle_rate 1.01.
        //   gross = 1% × $100 = $1.00; cost = $0.058 → net = $0.942.
        pr.insert(
            "SushiSwap".into(),
            m(&[("AAA-BBB", 1.0), ("BBB-AAA", 1.0)]),
        );

        let (_n, _edges, econ, adj) = extract_edges(&pr, &AdjCostParams::default());

        // Sem dedup seriam 4 adj (2 espelhos × 2 ciclos). Dedup colapsa espelhos → 2.
        assert_eq!(adj.len(), 2, "espelhos A-B/B-A devem colapsar em 1 adj cada");

        // gross/adj contam ÚNICOS pós-dedup (2 ciclos), não 4 direções.
        assert_eq!(econ.gross_positive, 2, "gross_positive pós-dedup = 2 ciclos únicos");
        assert_eq!(econ.venue_fee_adjusted_positive, 2, "adj == gross por construção");

        // net_positive: só o ciclo 2 (AAA-BBB) tem net>0. Ciclo 1 é net-negativo.
        assert_eq!(econ.net_positive, 1, "só 1 ciclo com net projetado > 0");

        // O ciclo net-positivo tem net>0; o net-negativo tem net<0.
        let caxas = adj.iter().find(|a| a.pair == "AAA-BBB").unwrap();
        assert!(caxas.net_profit_usd > 0.0, "AAA-BBB net={:.4} deve ser > 0", caxas.net_profit_usd);
        let vitrine = adj.iter().find(|a| a.pair == "USDC-USDT").unwrap();
        assert!(vitrine.net_profit_usd < 0.0, "USDC-USDT net={:.4} deve ser < 0", vitrine.net_profit_usd);
    }

    /// M12: round-trip negativo visto em A-B e B-A é um único ciclo econômico.
    #[test]
    fn extract_edges_dedup_mirror_negative_cycles() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // Melhor ciclo cruzado nos dois sentidos = 0.99 × 0.98 < 1.0.
        pr.insert(
            "QuickSwap".into(),
            m(&[("AAA-BBB", 0.99), ("BBB-AAA", 0.99)]),
        );
        pr.insert(
            "SushiSwap".into(),
            m(&[("AAA-BBB", 0.98), ("BBB-AAA", 0.98)]),
        );

        let (_n, _edges, econ, adj) = extract_edges(&pr, &AdjCostParams::default());

        assert!(adj.is_empty());
        assert_eq!(econ.evaluated, 2, "A-B e B-A são ambos avaliados");
        assert_eq!(
            econ.negative_cycles_found, 1,
            "espelhos negativos devem contar como um ciclo"
        );
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

        let (_n, edges, econ, _adj) = extract_edges(&pr, &AdjCostParams::default());
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

        let (_n, edges, _, _adj) = extract_edges(&pr, &AdjCostParams::default());
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

    // ===== TOP-N SPREAD (analyze_pair_spread / best_two_hop / tui_spread) =====

    /// Spread% single-dir = (max-min)/min*100, idêntico à fórmula do TUI
    /// (`tui.rs:221-224`).
    #[test]
    fn tui_spread_matches_tui_formula() {
        let fwd = vec![
            ("QuickSwap".to_string(), 0.000274),
            ("SushiSwap".to_string(), 0.000279),
            ("UniswapV3".to_string(), 0.000271),
        ];
        let spread = tui_spread_pct(&fwd);
        let expected = (0.000279 - 0.000271) / 0.000271 * 100.0;
        assert!((spread - expected).abs() < 1e-9, "spread={spread} expected={expected}");
        // <2 venues → 0 (igual TUI).
        assert_eq!(tui_spread_pct(&[("A".into(), 1.0)]), 0.0);
    }

    /// top-N revela também os ≤1.0 (diferente de extract_edges que só conta >1).
    /// cycle_rate 0.9987 ainda retorna leg1/leg2/cycle_rate.
    #[test]
    fn analyze_pair_spread_best_two_hop_ignores_gt1_filter() {
        let forward = vec![("QuickSwap".to_string(), 0.000274)];
        // reverse: WETH→DAI = 3648.0 → cycle_rate = 0.000274 × 3648.0 = 0.999552 (<1)
        let reverse = vec![("SushiSwap".to_string(), 3648.0)];
        let info = analyze_pair_spread("DAI-WETH", &forward, &reverse, &AdjCostParams::default());
        assert!(info.leg1.is_some(), "leg1 deve existir mesmo com cycle_rate < 1");
        assert!(info.leg2.is_some(), "leg2 deve existir");
        let cr = info.cycle_rate.expect("cycle_rate");
        assert!(cr < 1.0, "cycle_rate={cr} deve ser < 1 (revela não-fechamento)");
        assert!(info.gross_pct.unwrap() < 0.0, "gross deve ser negativo");
        // extract_edges NÃO emite edge <1, mas analyze_pair_spread retorna o ciclo.
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        pr.insert("QuickSwap".into(), m(&[("DAI-WETH", 0.000274), ("WETH-DAI", 3700.0)]));
        pr.insert("SushiSwap".into(), m(&[("DAI-WETH", 0.000274), ("WETH-DAI", 3648.0)]));
        let (_n, edges, _, _) = extract_edges(&pr, &AdjCostParams::default());
        // Com cycle_rate < 1 (best = 0.000274 × 3648 = 0.9996), extract_edges não push.
        assert!(edges.iter().all(|e| e.spread_pct > 0.0));
    }

    /// Sem reverse cotado → leg2=None, cycle_rate=None (razão do não-fechamento).
    #[test]
    fn analyze_pair_spread_no_reverse() {
        let forward = vec![
            ("QuickSwap".to_string(), 1.0),
            ("SushiSwap".to_string(), 1.02),
        ];
        let info = analyze_pair_spread("DAI-USDC", &forward, &[], &AdjCostParams::default());
        assert!(info.leg1.is_none(), "sem reverse → sem 2-hop → leg1=None");
        assert!(info.leg2.is_none());
        assert!(info.cycle_rate.is_none(), "sem cycle_rate sem reverse");
        assert!(info.gross_pct.is_none());
        // outlier ainda aponta o venue destoante no forward.
        assert_eq!(info.outlier.as_deref(), Some("SushiSwap"));
        assert!((info.tui_spread_pct - 2.0).abs() < 1e-9, "spread 2%");
    }

    /// Outlier = venue cujo preço mais se afasta da mediana forward.
    /// [1.0, 1.0, 1.02] → mediana 1.0, outlier = o 1.02.
    #[test]
    fn analyze_pair_spread_outlier_is_median_deviant() {
        let forward = vec![
            ("QuickSwap".to_string(), 1.0),
            ("SushiSwap".to_string(), 1.0),
            ("UniswapV3".to_string(), 1.02),
        ];
        let info = analyze_pair_spread("X-Y", &forward, &[], &AdjCostParams::default());
        assert_eq!(info.outlier.as_deref(), Some("UniswapV3"));
    }

    /// leg Curve → has_curve_leg=true, executable=false; QuickSwap+SushiSwap →
    /// executable=true.
    #[test]
    fn analyze_pair_spread_executable_and_curve_flags() {
        // QuickSwap (forward) × SushiSwap (reverse): ambos Cpmm executáveis.
        let fwd = vec![("QuickSwap".to_string(), 1.01)];
        let rev = vec![("SushiSwap".to_string(), 1.0)];
        let info = analyze_pair_spread("A-B", &fwd, &rev, &AdjCostParams::default());
        assert!(info.executable, "Quick×Sushi deve ser executável");
        assert!(!info.has_curve_leg, "sem Curve");

        // Curve (forward) × QuickSwap (reverse): Curve não executável pelo contrato.
        let fwd_c = vec![("Curve".to_string(), 1.0001)];
        let rev_c = vec![("QuickSwap".to_string(), 1.0001)];
        let info_c = analyze_pair_spread("USDC-USDT", &fwd_c, &rev_c, &AdjCostParams::default());
        assert!(!info_c.executable, "perna Curve → não executável");
        assert!(info_c.has_curve_leg, "tem perna Curve");
    }

    /// Helper best_two_hop extraído não muda o ciclo existente: extract_edges
    /// ainda emite os mesmos edges (>1.0) usando o mesmo helper.
    #[test]
    fn best_two_hop_shared_with_extract_edges() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        pr.insert("QuickSwap".into(), m(&[("AAA-BBB", 1.0), ("BBB-AAA", 1.0)]));
        pr.insert("SushiSwap".into(), m(&[("AAA-BBB", 1.01), ("BBB-AAA", 1.0)]));
        let (n, edges, econ, adj) = extract_edges(&pr, &AdjCostParams::default());
        assert!(!edges.is_empty(), "best_two_hop compartilhado mantém edges");
        assert_eq!(adj.len(), 1, "1 ciclo único (dedup mirror)");
        assert_eq!(econ.gross_positive, 1);
        // best_two_hop direto no mesmo forward/reverse dá o mesmo cycle_rate.
        let fwd = vec![("QuickSwap".to_string(), 1.0), ("SushiSwap".to_string(), 1.01)];
        let rev = vec![("QuickSwap".to_string(), 1.0), ("SushiSwap".to_string(), 1.0)];
        let best = best_two_hop(&fwd, &rev);
        let (bv, _bp, sv, _sp, rate) = best.unwrap();
        assert_eq!(bv, "SushiSwap");
        assert_eq!(sv, "QuickSwap");
        assert!((rate - 1.01).abs() < 1e-9, "rate={rate}");
        let _ = n;
    }

    /// TVL formatação: $1.2M / $340k / $123 / tvl=? (None/inválido).
    #[test]
    fn fmt_tvl_compact_tiers() {
        assert_eq!(fmt_tvl(Some(1_200_000.0)), "tvl=$1.2M");
        assert_eq!(fmt_tvl(Some(340_000.0)), "tvl=$340k");
        assert_eq!(fmt_tvl(Some(123.0)), "tvl=$123");
        assert_eq!(fmt_tvl(None), "tvl=?");
        assert_eq!(fmt_tvl(Some(0.0)), "tvl=?");
        assert_eq!(fmt_tvl(Some(f64::NAN)), "tvl=?");
    }

    // ===== compute_top_spreads (sync, sem TVL) =====

    /// n=0 → vazio. Sem TVL/RPC (fn sync).
    #[test]
    fn compute_top_spreads_n_zero_vazio() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        pr.insert("QuickSwap".into(), m(&[("A-B", 1.0), ("B-A", 1.0)]));
        pr.insert("SushiSwap".into(), m(&[("A-B", 1.02), ("B-A", 1.0)]));
        assert!(compute_top_spreads(&pr, &AdjCostParams::default(), 0).is_empty());
    }

    /// Ordenado desc por net_usd, ≤n, sem campo TVL (TopSpreadInfo não tem).
    #[test]
    fn compute_top_spreads_sync_sem_tvl_ordenado_desc() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // A-B: spread 2% (1.0 vs 1.02)
        pr.insert("QuickSwap".into(), m(&[("A-B", 1.0), ("B-A", 1.0)]));
        pr.insert("SushiSwap".into(), m(&[("A-B", 1.02), ("B-A", 1.0)]));
        // C-D: spread 1% (1.0 vs 1.01)
        pr.insert("UniswapV3".into(), m(&[("C-D", 1.0), ("D-C", 1.0)]));
        // segundo venue C-D via Sushi (merge no mesmo insert não sobrescreve — uso QuickSwap)
        pr.insert(
            "QuickSwap".into(),
            m(&[("A-B", 1.0), ("B-A", 1.0), ("C-D", 1.01), ("D-C", 1.0)]),
        );

        let rows = compute_top_spreads(&pr, &AdjCostParams::default(), 5);
        assert!(rows.len() <= 5);
        assert!(!rows.is_empty(), "deve haver ≥1 spread");
        // Ordenado por net USD desc; spread single-dir é apenas informativo.
        for w in rows.windows(2) {
            assert!(
                w[0].net_usd.unwrap_or(f64::NEG_INFINITY)
                    >= w[1].net_usd.unwrap_or(f64::NEG_INFINITY),
                "net deve ser desc: {:?} < {:?}",
                w[0].net_usd,
                w[1].net_usd
            );
        }
        // TopSpreadInfo não carrega TVL — struct não tem campo tvl (compilação garante).
        let _ = rows[0].pair.clone(); // acessível (pub)
    }

    /// Par sem reverse cotado não entra no ranking por net_usd.
    #[test]
    fn compute_top_spreads_no_reverse_cycle_none() {
        let mut pr: HashMap<String, HashMap<String, f64>> = HashMap::new();
        // X-Y forward em 2 venues, mas sem Y-X em nenhuma → sem reverse.
        pr.insert("QuickSwap".into(), m(&[("X-Y", 1.0)]));
        pr.insert("SushiSwap".into(), m(&[("X-Y", 1.05)]));
        let rows = compute_top_spreads(&pr, &AdjCostParams::default(), 5);
        assert!(
            rows.iter().all(|r| r.pair != "X-Y"),
            "sem reverse não tem net_usd e não deve entrar no ranking"
        );
    }
}
