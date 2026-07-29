// ============================================================
// src/core/bf_graph.rs — Bellman-Ford Graph Detection
// ============================================================
//
// Detecta ciclos de arbitragem em grafo completo via Bellman-Ford
// com pesos em log-space (-ln(rate)). Complementa a busca
// combinatória (direct + triangular) com detecção de ciclos
// de N hops genéricos.
//
// Cada vértice = token, cada aresta = quote fee-inclusive direcional.
// NÃO cria aresta reversa automaticamente (AMMs não têm simetria).
//
// ============================================================

use std::collections::{HashMap, HashSet};

/// Vértice do grafo — identificado por índice.
pub type VertexId = usize;

/// Aresta direcionada com taxa fee-inclusive do quoter.
#[derive(Debug, Clone)]
pub struct PriceEdge {
    pub from: VertexId,
    pub to: VertexId,
    pub dex_name: String,
    pub rate: f64,
    pub token_in: String,
    pub token_out: String,
}

/// Grafo de preços para detecção de arbitragem.
#[derive(Debug, Clone)]
pub struct PriceGraph {
    pub tokens: Vec<String>,
    pub edges: Vec<PriceEdge>,
}

impl PriceGraph {
    /// Constrói grafo a partir do price_map.
    /// Cada par `tokenA-tokenB` com taxa > 0 vira uma aresta direcionada.
    /// NÃO cria aresta reversa automaticamente (correto para AMMs).
    pub fn from_price_map(prices: &HashMap<String, HashMap<String, f64>>) -> Self {
        let mut tokens: Vec<String> = Vec::new();
        let mut token_to_idx: HashMap<String, usize> = HashMap::new();
        let mut edges: Vec<PriceEdge> = Vec::new();

        let mut get_or_insert_idx = |token: &str| -> usize {
            if let Some(&idx) = token_to_idx.get(token) {
                return idx;
            }
            let idx = tokens.len();
            tokens.push(token.to_string());
            token_to_idx.insert(token.to_string(), idx);
            idx
        };

        for (dex_name, dex_prices) in prices {
            for (pair, &rate) in dex_prices {
                if !rate.is_finite() || rate <= 0.0 {
                    continue;
                }
                let parts: Vec<&str> = pair.split('-').collect();
                if parts.len() != 2 {
                    continue;
                }
                let token_a = parts[0];
                let token_b = parts[1];
                let from = get_or_insert_idx(token_a);
                let to = get_or_insert_idx(token_b);

                if from != to {
                    edges.push(PriceEdge {
                        from,
                        to,
                        dex_name: dex_name.clone(),
                        rate,
                        token_in: token_a.to_string(),
                        token_out: token_b.to_string(),
                    });
                }
            }
        }

        PriceGraph { tokens, edges }
    }

    /// Retorna índice do token, ou None.
    pub fn token_idx(&self, symbol: &str) -> Option<usize> {
        self.tokens
            .iter()
            .position(|t| t.eq_ignore_ascii_case(symbol))
    }
}

/// Resultado da detecção Bellman-Ford.
#[derive(Debug, Clone)]
pub struct BfCycle {
    /// Índices dos tokens no ciclo (fechado: último == primeiro).
    pub path: Vec<VertexId>,
    /// Nomes dos tokens.
    pub token_path: Vec<String>,
    /// Arestas do ciclo.
    pub edges: Vec<PriceEdge>,
    /// Produto das taxas (rate1 * rate2 * ...).
    pub product: f64,
    /// Spread percentual = (product - 1.0) * 100.
    pub spread_pct: f64,
    /// Soma dos pesos (-ln(rate)) — negativa = lucro.
    pub total_weight: f64,
}

/// Detecta ciclos de arbitragem via Bellman-Ford em log-space.
///
/// Algoritmo:
/// 1. Converte cada taxa para peso = -ln(rate) (multiplicativo → aditivo)
/// 2. Roda Bellman-Ford N-1 iterações de relaxamento
/// 3. N-ésima iteração detecta ciclos negativos (soma de pesos < 0)
/// 4. Reconstroi ciclo via predecessor array
/// 5. Calcula produto = e^(-soma_pesos) = product(rate)
///
/// Roda de cada vértice para cobrir grafos desconectados.
/// Retorna ciclos com spread entre min_spread_pct e max_spread_pct.
pub fn find_arbitrage_cycles(
    graph: &PriceGraph,
    min_spread_pct: f64,
    max_spread_pct: f64,
) -> Vec<BfCycle> {
    let n = graph.tokens.len();
    if n < 2 || graph.edges.is_empty() {
        return Vec::new();
    }

    let mut cycles = Vec::new();
    // Dedup: conjunto de ciclos já encontrados (tokens ordenados, sem o último que fecha)
    let mut seen_cycle_keys: HashSet<Vec<usize>> = HashSet::new();

    // Roda BF de cada vértice para cobrir grafos desconectados
    for start in 0..n {
        let mut dist = vec![0.0_f64; n];
        // (edge_index, prev_vertex)
        let mut pred: Vec<Option<(usize, usize)>> = vec![None; n];

        // N-1 relaxamentos
        for _ in 0..n.saturating_sub(1) {
            for (ei, edge) in graph.edges.iter().enumerate() {
                let w = -edge.rate.ln();
                let new_dist = dist[edge.from] + w;
                if new_dist < dist[edge.to] - 1e-12 {
                    dist[edge.to] = new_dist;
                    pred[edge.to] = Some((ei, edge.from));
                }
            }
        }

        // N-ésima iteração: detecta ciclo negativo
        for edge in &graph.edges {
            let w = -edge.rate.ln();
            if dist[edge.from] + w < dist[edge.to] - 1e-12 {
                // Ciclo negativo detectado!
                // Reconstroi ciclo: caminha de trás p/ frente até encontrar repetição
                let mut visited = HashSet::new();
                let mut cur = edge.from;

                // Encontra o vértice que está no ciclo
                let cycle_start = loop {
                    if !visited.insert(cur) {
                        break cur;
                    }
                    match pred[cur] {
                        Some((_, prev)) => cur = prev,
                        None => break edge.from,
                    }
                };

                // Reconstroi o ciclo a partir de cycle_start
                let mut cycle_vertices = Vec::new();
                let mut cur = cycle_start;
                loop {
                    cycle_vertices.push(cur);
                    match pred[cur] {
                        Some((_, prev)) => {
                            if prev == cycle_start {
                                break;
                            }
                            cur = prev;
                        }
                        None => break,
                    }
                }
                cycle_vertices.push(cycle_start); // fecha o ciclo
                cycle_vertices.reverse();

                // Remove duplicatas de reconstrução (mantém ordem)
                let mut unique = Vec::new();
                let mut seen = HashSet::new();
                for &v in &cycle_vertices {
                    if seen.insert(v) {
                        unique.push(v);
                    }
                }
                if unique.len() < 2 {
                    continue;
                }
                // Fecha o ciclo
                if unique.first() != unique.last() {
                    unique.push(unique[0]);
                }

                // Chave de dedup: ordenar os vértices (sem o último que é igual ao primeiro)
                let mut dedup_key = unique[..unique.len() - 1].to_vec();
                dedup_key.sort();
                if !seen_cycle_keys.insert(dedup_key) {
                    continue; // já detectado em outro start
                }

                // Calcula produto das taxas e coleta arestas do ciclo
                let mut product = 1.0;
                let mut cycle_edges = Vec::new();
                let mut cycle_valid = true;

                for w in unique.windows(2) {
                    let mut found = false;
                    // Tenta aresta direta w[0] → w[1]
                    for e in &graph.edges {
                        if e.from == w[0] && e.to == w[1] {
                            product *= e.rate;
                            cycle_edges.push(e.clone());
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        // Tenta inverso: w[1] → w[0] → rate = 1/rate_original
                        for e in &graph.edges {
                            if e.from == w[1] && e.to == w[0] {
                                let inv_rate = 1.0 / e.rate;
                                product *= inv_rate;
                                cycle_edges.push(PriceEdge {
                                    from: e.from,
                                    to: e.to,
                                    dex_name: e.dex_name.clone(),
                                    rate: inv_rate,
                                    token_in: e.token_out.clone(),
                                    token_out: e.token_in.clone(),
                                });
                                found = true;
                                break;
                            }
                        }
                    }
                    if !found {
                        cycle_valid = false;
                        break;
                    }
                }

                if !cycle_valid || !product.is_finite() || product <= 0.0 {
                    continue;
                }
                let spread = (product - 1.0) * 100.0;
                if spread < min_spread_pct || spread > max_spread_pct {
                    continue;
                }

                let token_path: Vec<String> =
                    unique.iter().map(|&i| graph.tokens[i].clone()).collect();

                cycles.push(BfCycle {
                    path: unique,
                    token_path,
                    edges: cycle_edges,
                    product,
                    spread_pct: spread,
                    total_weight: dist[edge.from] + w - dist[edge.to],
                });

                // Só um ciclo por start para evitar explosão de resultados
                break;
            }
        }
    }

    cycles
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn make_price_map(
        dex: &str,
        pairs: &[(&str, f64)],
    ) -> HashMap<String, HashMap<String, f64>> {
        let mut m = HashMap::new();
        let mut inner = HashMap::new();
        for (pair, rate) in pairs {
            inner.insert(pair.to_string(), *rate);
        }
        m.insert(dex.to_string(), inner);
        m
    }

    #[test]
    fn bf_detects_cross_dex_arbitrage() {
        // QuickSwap USDT→WMATIC = 7.14, UniswapV3 WMATIC→USDT = 0.145
        // cycle_rate = 7.14 * 0.145 = 1.0353 → +3.53%
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut qs = HashMap::new();
        qs.insert("USDT-WMATIC".into(), 7.14);
        prices.insert("QuickSwap".into(), qs);
        let mut uni = HashMap::new();
        uni.insert("WMATIC-USDT".into(), 0.145);
        prices.insert("UniswapV3".into(), uni);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(!cycles.is_empty(), "deve detectar ciclo lucrativo");
        assert!(
            (cycles[0].spread_pct - 3.53).abs() < 0.1,
            "spread ~3.53%, got {}",
            cycles[0].spread_pct
        );
    }

    #[test]
    fn bf_no_false_positive_on_efficient_market() {
        // Mesmo DEX: rates realistas → cycle_rate < 1.0
        let prices = make_price_map(
            "QuickSwap",
            &[("USDT-WMATIC", 7.14), ("WMATIC-USDT", 0.139)],
        );
        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(
            cycles.is_empty(),
            "não deve detectar lucro em mercado eficiente"
        );
    }

    #[test]
    fn bf_detects_triangular_arbitrage() {
        // USDC→LINK (0.06) * LINK→WETH (0.004) * WETH→USDC (4200) = 1.008
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut uni = HashMap::new();
        uni.insert("USDC-LINK".into(), 0.06);
        uni.insert("LINK-WETH".into(), 0.004);
        uni.insert("WETH-USDC".into(), 4200.0);
        prices.insert("UniswapV3".into(), uni);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(!cycles.is_empty(), "deve detectar triangular");
        assert!(
            (cycles[0].spread_pct - 0.8).abs() < 0.1,
            "spread ~0.8%, got {}",
            cycles[0].spread_pct
        );
    }

    #[test]
    fn bf_empty_graph_no_cycles() {
        let graph = PriceGraph {
            tokens: vec![],
            edges: vec![],
        };
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(cycles.is_empty());
    }

    #[test]
    fn bf_no_cycle_when_no_negative_loop() {
        // A→B→A onde product < 1.0
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut qs = HashMap::new();
        qs.insert("USDT-WMATIC".into(), 7.14);
        qs.insert("WMATIC-USDT".into(), 0.13); // 7.14 * 0.13 = 0.9282 < 1.0
        prices.insert("QuickSwap".into(), qs);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(cycles.is_empty());
    }

    #[test]
    fn bf_handles_disconnected_graphs() {
        // Dois componentes isolados — não deve panic
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut d1 = HashMap::new();
        d1.insert("USDT-USDC".into(), 1.001);
        prices.insert("DEX1".into(), d1);
        let mut d2 = HashMap::new();
        d2.insert("WMATIC-WETH".into(), 0.000047);
        d2.insert("WETH-WMATIC".into(), 21000.0);
        prices.insert("DEX2".into(), d2);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        // Não deve crashar — pode ou não ter ciclos
        assert!(cycles.len() < 100);
    }

    #[test]
    fn bf_4hop_profitable_no_crash() {
        // 4 arestas em 3 DEXes — não deve crashar
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();

        let mut qs = HashMap::new();
        qs.insert("USDT-WMATIC".into(), 7.3);
        qs.insert("USDC-USDT".into(), 1.001);
        prices.insert("QuickSwap".into(), qs);

        let mut uni = HashMap::new();
        uni.insert("WMATIC-WETH".into(), 0.0000475);
        prices.insert("UniswapV3".into(), uni);

        let mut sushi = HashMap::new();
        sushi.insert("WETH-USDC".into(), 2900.0);
        prices.insert("SushiSwap".into(), sushi);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(cycles.len() < 100);
    }

    #[test]
    fn bf_spread_filter_respects_min_max() {
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut qs = HashMap::new();
        qs.insert("USDT-WMATIC".into(), 7.14);
        prices.insert("QuickSwap".into(), qs);
        let mut uni = HashMap::new();
        uni.insert("WMATIC-USDT".into(), 0.145);
        prices.insert("UniswapV3".into(), uni);

        let graph = PriceGraph::from_price_map(&prices);

        // min_spread = 5% → deve filtrar (spread ~3.53%)
        let cycles_high_min = find_arbitrage_cycles(&graph, 5.0, 50.0);
        assert!(cycles_high_min.is_empty(), "min_spread 5% deve filtrar ~3.53%");

        // max_spread = 2% → deve filtrar (spread ~3.53%)
        let cycles_low_max = find_arbitrage_cycles(&graph, 0.1, 2.0);
        assert!(cycles_low_max.is_empty(), "max_spread 2% deve filtrar ~3.53%");
    }

    #[test]
    fn bf_cycle_path_has_correct_tokens() {
        let mut prices: HashMap<String, HashMap<String, f64>> = HashMap::new();
        let mut uni = HashMap::new();
        uni.insert("USDC-LINK".into(), 0.06);
        uni.insert("LINK-WETH".into(), 0.004);
        uni.insert("WETH-USDC".into(), 4200.0);
        prices.insert("UniswapV3".into(), uni);

        let graph = PriceGraph::from_price_map(&prices);
        let cycles = find_arbitrage_cycles(&graph, 0.1, 50.0);
        assert!(!cycles.is_empty());
        let c = &cycles[0];
        assert_eq!(c.token_path.len(), c.path.len());
        // Primeiro e último token devem ser iguais (ciclo fechado)
        assert_eq!(c.token_path.first(), c.token_path.last());
        // Deve ter pelo menos 3 tokens únicos + 1 fechamento
        assert!(c.token_path.len() >= 4);
    }
}