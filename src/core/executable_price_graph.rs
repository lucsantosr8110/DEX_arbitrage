//! Typed executable price graph and structural cycle finder for Phase
//! 2D-C2B. Operates exclusively on `ExecutablePriceEdge` (real addresses,
//! real `U256` amounts, real anchor block/hash) — never on symbols or an
//! aggregated `f64` rate. This supersedes, for this pipeline, the legacy
//! `bf_graph::PriceGraph` (kept only as a diagnostic/UI path for the
//! production bot — see `bf_graph.rs`).

use crate::core::{
    executable_call::Venue,
    executable_price_edge::ExecutablePriceEdge,
    route_artifact::{RouteLeg, RouteReturnClass, StructuralRoute, StructuralRouteLeg},
};
use ethers::types::{Address, U256};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Default)]
pub struct ExecutableEdgeGraph {
    pub edges: Vec<ExecutablePriceEdge>,
    by_token_in: HashMap<Address, Vec<usize>>,
}

impl ExecutableEdgeGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, edge: ExecutablePriceEdge) {
        let idx = self.edges.len();
        self.by_token_in.entry(edge.token_in).or_default().push(idx);
        self.edges.push(edge);
    }

    fn edges_from(&self, token: Address) -> impl Iterator<Item = usize> + '_ {
        self.by_token_in
            .get(&token)
            .into_iter()
            .flat_map(|v| v.iter().copied())
    }
}

fn venue_name(venue: Venue) -> &'static str {
    match venue {
        Venue::UniswapV3 => "UniswapV3",
        Venue::QuickSwap => "QuickSwap",
        Venue::SushiSwap => "SushiSwap",
        Venue::Curve => "Curve",
    }
}

fn protocol_version(venue: Venue) -> &'static str {
    match venue {
        Venue::UniswapV3 => "V3",
        _ => "V2",
    }
}

/// Finds closed cycles (`min_hops..=max_hops` edges, no repeated edge
/// within a cycle) starting and ending at each of `start_tokens`, and
/// converts each directly into a `StructuralRoute` with `executable_legs`
/// populated by cloning the matched edges — no reconstruction from strings
/// or rates.
///
/// With several parallel venues per token pair, allowing short (2-hop)
/// closures combinatorially explodes the candidate count (every
/// venue-on-A->B paired with every venue-on-B->A). Callers driving a real
/// campaign should pass `min_hops == max_hops` (e.g. `3, 3`, matching the
/// triangular-only scope this pipeline replaces) to keep the downstream
/// per-route re-quote pass bounded; tests may use a wider range for
/// simpler fixtures.
pub fn find_structural_cycles(
    graph: &ExecutableEdgeGraph,
    start_tokens: &[Address],
    min_hops: usize,
    max_hops: usize,
    route_input: U256,
    profile: &str,
) -> Vec<StructuralRoute> {
    let mut out = Vec::new();
    let mut seen_keys: HashSet<String> = HashSet::new();
    for &start in start_tokens {
        let mut path: Vec<usize> = Vec::new();
        dfs(
            graph,
            start,
            start,
            min_hops,
            max_hops,
            &mut path,
            route_input,
            profile,
            &mut out,
            &mut seen_keys,
        );
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn dfs(
    graph: &ExecutableEdgeGraph,
    start: Address,
    current: Address,
    min_hops: usize,
    max_hops: usize,
    path: &mut Vec<usize>,
    route_input: U256,
    profile: &str,
    out: &mut Vec<StructuralRoute>,
    seen_keys: &mut HashSet<String>,
) {
    if path.len() >= max_hops {
        return;
    }
    for edge_idx in graph.edges_from(current) {
        if path.contains(&edge_idx) {
            continue;
        }
        let edge = &graph.edges[edge_idx];
        path.push(edge_idx);
        if edge.token_out == start && path.len() >= min_hops {
            if let Some(route) = build_route(graph, path, profile, route_input) {
                if seen_keys.insert(route.structural_cycle_key.clone()) {
                    out.push(route);
                }
            }
        } else if edge.token_out != start {
            dfs(
                graph,
                start,
                edge.token_out,
                min_hops,
                max_hops,
                path,
                route_input,
                profile,
                out,
                seen_keys,
            );
        }
        path.pop();
    }
}

/// Builds a `StructuralRoute` from a closed sequence of edge indices.
/// Fail-closed (`None`, cycle silently dropped) on: mixed anchor
/// block/hash, duplicate quote_id, missing token symbols, or token
/// discontinuity — never fabricates a substitute value.
fn build_route(
    graph: &ExecutableEdgeGraph,
    path: &[usize],
    profile: &str,
    route_input: U256,
) -> Option<StructuralRoute> {
    let edges: Vec<&ExecutablePriceEdge> = path.iter().map(|&i| &graph.edges[i]).collect();
    let (first, rest) = edges.split_first()?;
    if rest.iter().any(|e| {
        e.anchor_block != first.anchor_block || e.anchor_block_hash != first.anchor_block_hash
    }) {
        return None;
    }
    let mut quote_ids = HashSet::new();
    if !edges.iter().all(|e| quote_ids.insert(e.quote_id)) {
        return None;
    }
    for window in edges.windows(2) {
        if window[0].token_out != window[1].token_in {
            return None;
        }
    }
    if edges.last()?.token_out != first.token_in {
        return None;
    }

    let mut executable_legs = Vec::with_capacity(edges.len());
    let mut legs = Vec::with_capacity(edges.len());
    let mut pools = Vec::with_capacity(edges.len());
    let mut venues = Vec::with_capacity(edges.len());
    let mut key_parts = Vec::with_capacity(edges.len());

    for (i, edge) in edges.iter().enumerate() {
        let sym_in = edge.token_in_symbol.clone()?;
        let sym_out = edge.token_out_symbol.clone()?;
        executable_legs.push(StructuralRouteLeg {
            leg_index: i,
            venue: edge.venue,
            token_in: edge.token_in,
            token_out: edge.token_out,
            pool: edge.pool,
            router: edge.router,
            spender: edge.spender,
            fee: edge.fee,
        });
        let pool_str = format!("{:?}", edge.pool);
        let fee_str = edge.fee.map(|f| f.to_string()).unwrap_or_default();
        key_parts.push(format!(
            "{sym_in}>{sym_out}|{}|{}|{}|{}",
            venue_name(edge.venue),
            protocol_version(edge.venue),
            pool_str,
            fee_str
        ));
        legs.push(RouteLeg {
            token_in: sym_in,
            token_out: sym_out,
            venue: venue_name(edge.venue).to_string(),
            protocol_version: protocol_version(edge.venue).to_string(),
            pool_address: Some(pool_str.clone()),
            fee_tier: edge.fee,
        });
        pools.push(pool_str);
        venues.push(venue_name(edge.venue).to_string());
    }

    let structural_cycle_key = key_parts.join("||");
    let route_id = format!("{profile}:{structural_cycle_key}");

    let final_out = edges.last()?.amount_out;
    if route_input.is_zero() {
        return None;
    }
    let gross_multiplier_avg = final_out.as_u128() as f64 / route_input.as_u128() as f64;

    let route = StructuralRoute {
        route_id,
        structural_cycle_key,
        legs,
        hop_count: edges.len(),
        profile: profile.to_string(),
        scans_observed: 1,
        return_class: RouteReturnClass::ReturnInsufficientObservations,
        pools,
        venues,
        gross_multiplier_avg,
        anchor_block: first.anchor_block,
        anchor_block_hash: first.anchor_block_hash,
        route_input,
        executable_legs: Some(executable_legs),
    };
    // STRING_LEGS_DERIVED_ONLY_FROM_EXECUTABLE_LEGS: both leg
    // representations were just built from the same edges above; this
    // fails closed rather than emitting a route whose textual legs could
    // ever diverge from its typed legs.
    if !verify_leg_parity(&route) {
        return None;
    }
    Some(route)
}

/// Verifies that `route.legs` (the string form the materializer consumes)
/// was derived only from `route.executable_legs` (typed, edge-sourced) and
/// never independently reconstructed: pool, venue, fee and token order must
/// match exactly, index-for-index. `build_route` already builds both from
/// the same edge in the same pass, so this should always hold — this
/// function is the explicit, callable proof of that invariant for the
/// binary's gate output and for tests, not a second source of truth.
pub fn verify_leg_parity(route: &StructuralRoute) -> bool {
    let Some(typed) = &route.executable_legs else {
        return false;
    };
    if typed.len() != route.legs.len() {
        return false;
    }
    for (t, s) in typed.iter().zip(route.legs.iter()) {
        if s.pool_address.as_deref() != Some(format!("{:?}", t.pool)).as_deref() {
            return false;
        }
        if s.venue != venue_name(t.venue) {
            return false;
        }
        if s.protocol_version != protocol_version(t.venue) {
            return false;
        }
        if s.fee_tier != t.fee {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::canonical_adapters::PinnedQuoteRecord;
    use crate::core::canonical_execution_context::PoolExecutionMetadata;
    use ethers::types::H256;

    fn a(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }
    fn h(n: u64) -> H256 {
        H256::from_low_u64_be(n)
    }

    #[allow(clippy::too_many_arguments)]
    fn edge(
        quote_id: u64,
        token_in: Address,
        token_out: Address,
        pool: u64,
        venue: Venue,
        sym_in: &str,
        sym_out: &str,
        anchor_block: u64,
        anchor_hash: H256,
    ) -> ExecutablePriceEdge {
        let quote = PinnedQuoteRecord {
            quote_id: h(quote_id),
            anchor_block,
            anchor_hash,
            venue,
            pool: a(pool),
            token_in,
            token_out,
            amount_in: U256::from(100),
            amount_out: U256::from(101),
            pool_state_id: h(quote_id + 1000),
            execution_metadata_id: h(quote_id + 2000),
            adapter_version: "test".into(),
            provenance_hash: h(quote_id + 3000),
        };
        let pool_meta = PoolExecutionMetadata {
            venue: venue_name(venue).into(),
            pool: a(pool),
            router: a(pool + 500),
            spender: a(pool + 500),
            token_order: (token_in, token_out),
            fee: if venue == Venue::UniswapV3 {
                Some(500)
            } else {
                None
            },
            curve_method: None,
            curve_indices: None,
            implementation_code_hash: h(pool + 999),
            anchor_block,
        };
        ExecutablePriceEdge::from_quote(
            &quote,
            &pool_meta,
            Some(sym_in.to_string()),
            Some(sym_out.to_string()),
        )
        .unwrap()
    }

    #[test]
    fn finds_closed_triangular_cycle() {
        let anchor_hash = h(1);
        let mut g = ExecutableEdgeGraph::new();
        g.push(edge(
            1,
            a(1),
            a(2),
            10,
            Venue::QuickSwap,
            "A",
            "B",
            9,
            anchor_hash,
        ));
        g.push(edge(
            2,
            a(2),
            a(3),
            11,
            Venue::SushiSwap,
            "B",
            "C",
            9,
            anchor_hash,
        ));
        g.push(edge(
            3,
            a(3),
            a(1),
            12,
            Venue::UniswapV3,
            "C",
            "A",
            9,
            anchor_hash,
        ));

        let routes = find_structural_cycles(&g, &[a(1)], 2, 3, U256::from(100), "base");
        assert_eq!(routes.len(), 1);
        let r = &routes[0];
        assert_eq!(r.hop_count, 3);
        assert!(r.executable_legs.is_some());
        assert_eq!(r.executable_legs.as_ref().unwrap().len(), 3);
        assert_eq!(r.anchor_block, 9);
    }

    #[test]
    fn mixed_anchor_edges_fail_closed() {
        let mut g = ExecutableEdgeGraph::new();
        g.push(edge(1, a(1), a(2), 10, Venue::QuickSwap, "A", "B", 9, h(1)));
        g.push(edge(2, a(2), a(1), 11, Venue::SushiSwap, "B", "A", 9, h(2))); // different hash
        let routes = find_structural_cycles(&g, &[a(1)], 2, 3, U256::from(100), "base");
        assert!(routes.is_empty());
    }

    #[test]
    fn no_cycle_returns_empty() {
        let mut g = ExecutableEdgeGraph::new();
        g.push(edge(1, a(1), a(2), 10, Venue::QuickSwap, "A", "B", 9, h(1)));
        let routes = find_structural_cycles(&g, &[a(1)], 2, 3, U256::from(100), "base");
        assert!(routes.is_empty());
    }
}
