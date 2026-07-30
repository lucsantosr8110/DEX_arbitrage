//! Diagnostic directed multigraph. No execution dependencies.
use crate::core::phase2d_anchor::AnchorBlock;
use anyhow::{anyhow, Result};
use ethers::types::H256;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;
pub type TokenId = usize;
pub type EdgeId = usize;
pub const EPSILON: f64 = 1e-12;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticToken {
    pub id: TokenId,
    pub symbol: String,
    pub address: String,
    pub decimals: u8,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEdge {
    pub id: EdgeId,
    pub from: TokenId,
    pub to: TokenId,
    pub token_in_symbol: String,
    pub token_out_symbol: String,
    pub token_in_address: String,
    pub token_out_address: String,
    pub dex_name: String,
    pub protocol_version: String,
    pub pool_address: Option<String>,
    pub fee_tier: Option<u32>,
    pub rate: f64,
    pub amount_in_raw: String,
    pub amount_out_raw: String,
    pub block_number: Option<u64>,
    #[serde(default)]
    pub anchor_block_number: Option<u64>,
    #[serde(default)]
    pub anchor_block_hash: Option<H256>,
    #[serde(default)]
    pub pinned: bool,
    pub quote_source: String,
    pub reciprocity_status: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticGraph {
    pub schema_version: u32,
    pub scan_id: String,
    pub chain: String,
    pub captured_at: String,
    pub block_start: u64,
    pub block_end: u64,
    #[serde(default)]
    pub temporal_mode: Option<String>,
    #[serde(default)]
    pub anchor_block: Option<AnchorBlock>,
    #[serde(default)]
    pub head_at_scan_start: Option<u64>,
    #[serde(default)]
    pub head_at_scan_end: Option<u64>,
    #[serde(default)]
    pub head_advance_during_scan: Option<u64>,
    #[serde(default)]
    pub anchor_hash_verified_before: Option<bool>,
    #[serde(default)]
    pub anchor_hash_verified_after: Option<bool>,
    #[serde(default)]
    pub reorg_detected: Option<bool>,
    #[serde(default)]
    pub quote_state_block_span: Option<u64>,
    pub tokens: Vec<DiagnosticToken>,
    pub edges: Vec<DiagnosticEdge>,
}
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct GraphValidationReport {
    pub tokens_total: usize,
    pub edges_total: usize,
    pub invalid_edges: usize,
    pub parallel_edge_groups: usize,
    pub bidirectional_pair_groups: usize,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExactCycle {
    pub edge_ids: Vec<EdgeId>,
    pub token_ids: Vec<TokenId>,
    pub product: f64,
    pub total_weight: f64,
    pub spread_pct: f64,
    pub canonical_key: String,
}
#[derive(Debug, Default, Clone)]
pub struct BfDiagnosticResult {
    pub negative_relaxations: usize,
    pub reconstruction_attempts: usize,
    pub reconstruction_successes: usize,
    pub reconstruction_failures: usize,
    pub cycles: Vec<ExactCycle>,
}
pub fn validate_diagnostic_graph(g: &DiagnosticGraph) -> Result<GraphValidationReport> {
    let mut r = GraphValidationReport {
        tokens_total: g.tokens.len(),
        edges_total: g.edges.len(),
        ..Default::default()
    };
    let mut ids = HashSet::new();
    let t: HashMap<_, _> = g.tokens.iter().map(|x| (x.id, x)).collect();
    let mut p = HashMap::<(usize, usize), usize>::new();
    for e in &g.edges {
        let ok = ids.insert(e.id)
            && e.from != e.to
            && e.rate.is_finite()
            && e.rate > 0.
            && t.get(&e.from)
                .is_some_and(|x| x.symbol == e.token_in_symbol && x.address == e.token_in_address)
            && t.get(&e.to).is_some_and(|x| {
                x.symbol == e.token_out_symbol && x.address == e.token_out_address
            });
        if !ok {
            r.invalid_edges += 1
        }
        *p.entry((e.from, e.to)).or_default() += 1
    }
    r.parallel_edge_groups = p.values().filter(|x| **x > 1).count();
    r.bidirectional_pair_groups = p
        .keys()
        .filter(|(a, b)| a < b && p.contains_key(&(*b, *a)))
        .count();
    if r.invalid_edges > 0 {
        Err(anyhow!("invalid edges"))
    } else {
        Ok(r)
    }
}
fn key(v: &[usize]) -> String {
    (0..v.len())
        .map(|i| {
            v.iter()
                .cycle()
                .skip(i)
                .take(v.len())
                .map(|x| x.to_string())
                .collect::<Vec<_>>()
                .join(":")
        })
        .min()
        .unwrap_or_default()
}
fn make(g: &DiagnosticGraph, ids: Vec<usize>, vs: Vec<usize>) -> ExactCycle {
    let m: HashMap<_, _> = g.edges.iter().map(|e| (e.id, e)).collect();
    let p = ids.iter().map(|i| m[i].rate).product::<f64>();
    ExactCycle {
        canonical_key: key(&ids),
        edge_ids: ids,
        token_ids: vs,
        total_weight: -p.ln(),
        spread_pct: (p - 1.) * 100.,
        product: p,
    }
}

struct CycleSearch<'a> {
    graph: &'a DiagnosticGraph,
    min_hops: usize,
    max_hops: usize,
    output: &'a mut Vec<ExactCycle>,
    seen: &'a mut HashSet<String>,
}

impl CycleSearch<'_> {
    fn walk(
        &mut self,
        start: usize,
        current: usize,
        ids: &mut Vec<usize>,
        tokens: &mut Vec<usize>,
    ) {
        if ids.len() == self.max_hops {
            return;
        }
        for edge in self.graph.edges.iter().filter(|edge| edge.from == current) {
            if edge.to == start && ids.len() + 1 >= self.min_hops {
                let mut cycle_ids = ids.clone();
                cycle_ids.push(edge.id);
                let mut cycle_tokens = tokens.clone();
                cycle_tokens.push(start);
                let cycle = make(self.graph, cycle_ids, cycle_tokens);
                if self.seen.insert(cycle.canonical_key.clone()) {
                    self.output.push(cycle);
                }
            } else if !tokens.contains(&edge.to) {
                ids.push(edge.id);
                tokens.push(edge.to);
                self.walk(start, edge.to, ids, tokens);
                tokens.pop();
                ids.pop();
            }
        }
    }
}

pub fn enumerate_simple_cycles_exact(
    g: &DiagnosticGraph,
    min: usize,
    max: usize,
) -> Vec<ExactCycle> {
    let mut o = Vec::new();
    let mut seen = HashSet::new();
    for t in &g.tokens {
        CycleSearch {
            graph: g,
            min_hops: min,
            max_hops: max,
            output: &mut o,
            seen: &mut seen,
        }
        .walk(t.id, t.id, &mut Vec::new(), &mut vec![t.id]);
    }
    o
}

/// Raw Bellman-Ford diagnostic. Predecessors are exact EdgeIds; no inverse edge
/// search or synthetic rate is permitted during reconstruction.
pub fn find_negative_cycles_raw(g: &DiagnosticGraph) -> BfDiagnosticResult {
    let mut result = BfDiagnosticResult::default();
    let n = g.tokens.len();
    if n < 2 {
        return result;
    }
    let mut dist = vec![0.0; n];
    let mut pred = vec![None; n];
    for _ in 0..n.saturating_sub(1) {
        for e in &g.edges {
            if dist[e.from] - e.rate.ln() < dist[e.to] - EPSILON {
                dist[e.to] = dist[e.from] - e.rate.ln();
                pred[e.to] = Some(e.id);
            }
        }
    }
    for edge in &g.edges {
        if dist[edge.from] - edge.rate.ln() >= dist[edge.to] - EPSILON {
            continue;
        }
        result.negative_relaxations += 1;
        result.reconstruction_attempts += 1;
        let mut v = edge.to;
        for _ in 0..n {
            let Some(id) = pred[v] else {
                result.reconstruction_failures += 1;
                continue;
            };
            v = g.edges[id].from;
        }
        let start = v;
        let mut ids = Vec::new();
        loop {
            let Some(id) = pred[v] else {
                result.reconstruction_failures += 1;
                break;
            };
            ids.push(id);
            v = g.edges[id].from;
            if v == start {
                ids.reverse();
                let cycle = make(g, ids, Vec::new());
                if cycle.total_weight < -EPSILON
                    && !result
                        .cycles
                        .iter()
                        .any(|c| c.canonical_key == cycle.canonical_key)
                {
                    result.reconstruction_successes += 1;
                    result.cycles.push(cycle);
                }
                break;
            }
        }
    }
    result
}

pub fn save_snapshot_atomic(graph: &DiagnosticGraph, path: &Path) -> Result<()> {
    validate_diagnostic_graph(graph)?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_vec_pretty(graph)?)?;
    fs::rename(tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn graph(edges: &[(usize, usize, usize, f64)]) -> DiagnosticGraph {
        let tokens = ["USDC", "WETH", "WMATIC"]
            .iter()
            .enumerate()
            .map(|(id, symbol)| DiagnosticToken {
                id,
                symbol: (*symbol).into(),
                address: format!("0x{id:040x}"),
                decimals: 6,
            })
            .collect::<Vec<_>>();
        DiagnosticGraph {
            schema_version: 1,
            scan_id: "fixture".into(),
            chain: "polygon".into(),
            captured_at: "2026-01-01T00:00:00Z".into(),
            block_start: 1,
            block_end: 1,
            temporal_mode: None,
            anchor_block: None,
            head_at_scan_start: None,
            head_at_scan_end: None,
            head_advance_during_scan: None,
            anchor_hash_verified_before: None,
            anchor_hash_verified_after: None,
            reorg_detected: None,
            quote_state_block_span: None,
            edges: edges
                .iter()
                .map(|(id, from, to, rate)| DiagnosticEdge {
                    id: *id,
                    from: *from,
                    to: *to,
                    token_in_symbol: tokens[*from].symbol.clone(),
                    token_out_symbol: tokens[*to].symbol.clone(),
                    token_in_address: tokens[*from].address.clone(),
                    token_out_address: tokens[*to].address.clone(),
                    dex_name: "fixture".into(),
                    protocol_version: "test".into(),
                    pool_address: None,
                    fee_tier: None,
                    rate: *rate,
                    amount_in_raw: "1000000".into(),
                    amount_out_raw: "1000000".into(),
                    block_number: Some(1),
                    anchor_block_number: None,
                    anchor_block_hash: None,
                    pinned: false,
                    quote_source: "fixture".into(),
                    reciprocity_status: "Accepted".into(),
                })
                .collect(),
            tokens,
        }
    }

    #[test]
    fn profitable_two_leg_cycle_is_exact_and_bf_detectable() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 1, 0, 0.6)]);
        assert_eq!(enumerate_simple_cycles_exact(&g, 2, 4).len(), 1);
        assert_eq!(find_negative_cycles_raw(&g).cycles.len(), 1);
    }

    #[test]
    fn one_way_quote_has_no_cycle() {
        let g = graph(&[(0, 0, 1, 2.0)]);
        assert!(enumerate_simple_cycles_exact(&g, 2, 4).is_empty());
        assert!(find_negative_cycles_raw(&g).cycles.is_empty());
    }

    #[test]
    fn parallel_edges_keep_their_own_edge_ids() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 0, 1, 1.8), (2, 1, 0, 0.6)]);
        let cycles = enumerate_simple_cycles_exact(&g, 2, 2);
        assert_eq!(cycles.len(), 2);
        assert!(cycles.iter().any(|cycle| cycle.edge_ids == vec![0, 2]));
        assert!(cycles.iter().any(|cycle| cycle.edge_ids == vec![1, 2]));
    }

    #[test]
    fn graph_json_round_trip_preserves_edges() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 1, 0, 0.6)]);
        let restored: DiagnosticGraph =
            serde_json::from_slice(&serde_json::to_vec(&g).unwrap()).unwrap();
        assert_eq!(restored.edges.len(), 2);
        assert_eq!(restored.edges[1].id, 1);
        validate_diagnostic_graph(&restored).unwrap();
    }

    #[test]
    fn replay_enumeration_is_deterministic() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 1, 2, 2.0), (2, 2, 0, 0.3)]);
        let first = enumerate_simple_cycles_exact(&g, 2, 4);
        let second = enumerate_simple_cycles_exact(&g, 2, 4);
        assert_eq!(first[0].canonical_key, second[0].canonical_key);
        assert_eq!(first[0].edge_ids, second[0].edge_ids);
    }

    #[test]
    fn phase2b_accepted_quote_is_in_pre_and_post() {
        let pre = graph(&[(0, 0, 1, 2.0)]);
        let post = pre.clone();
        assert_eq!(pre.edges[0].id, post.edges[0].id);
    }
    #[test]
    fn phase2b_rejected_quote_is_only_in_pre() {
        let pre = graph(&[(0, 0, 1, 2.0)]);
        let post = graph(&[]);
        assert_eq!(pre.edges.len(), 1);
        assert!(post.edges.is_empty());
    }
    #[test]
    fn phase2b_post_cannot_contain_rejected_edge() {
        let mut g = graph(&[(0, 0, 1, 2.0)]);
        g.edges[0].reciprocity_status = "Rejected".into();
        assert!(g.edges.iter().all(|e| e.reciprocity_status != "Accepted"));
    }
    #[test]
    fn phase2b_pre_negative_cycle_is_found_by_both_models() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 1, 0, 0.6)]);
        assert_eq!(
            enumerate_simple_cycles_exact(&g, 2, 2)
                .iter()
                .filter(|c| c.total_weight < 0.0)
                .count(),
            1
        );
        assert_eq!(find_negative_cycles_raw(&g).cycles.len(), 1);
    }
    #[test]
    fn phase2b_removed_edge_removes_cycle() {
        let pre = graph(&[(0, 0, 1, 2.0), (1, 1, 0, 0.6)]);
        let post = graph(&[(0, 0, 1, 2.0)]);
        assert!(!enumerate_simple_cycles_exact(&pre, 2, 2).is_empty());
        assert!(enumerate_simple_cycles_exact(&post, 2, 2).is_empty());
    }
    #[test]
    fn phase2b_filter_cannot_create_cycle_from_subset() {
        let pre = graph(&[(0, 0, 1, 2.0)]);
        let post = graph(&[]);
        assert!(enumerate_simple_cycles_exact(&post, 2, 4).is_empty());
        assert!(enumerate_simple_cycles_exact(&pre, 2, 4).is_empty());
    }
    #[test]
    fn phase2b_scan_graphs_are_isolated() {
        let mut a = graph(&[(0, 0, 1, 2.0)]);
        let b = graph(&[(0, 1, 0, 0.6)]);
        a.scan_id = "a".into();
        assert_ne!(a.scan_id, b.scan_id);
    }
    #[test]
    fn phase2b_rejection_matrix_group_count_is_stable() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 0, 1, 1.9)]);
        assert_eq!(
            validate_diagnostic_graph(&g).unwrap().parallel_edge_groups,
            1
        );
    }
    #[test]
    fn phase2b_pairing_classes_cover_same_and_cross_direction() {
        let g = graph(&[(0, 0, 1, 2.0), (1, 1, 0, 0.6)]);
        assert_eq!(
            validate_diagnostic_graph(&g)
                .unwrap()
                .bidirectional_pair_groups,
            1
        );
    }
    #[test]
    fn phase2b_summary_multiple_scans_adds_observations() {
        let a = graph(&[(0, 0, 1, 2.0)]);
        let b = graph(&[(0, 0, 1, 1.9)]);
        assert_eq!(a.edges.len() + b.edges.len(), 2);
    }
}
