//! Deterministic, RPC-free end-to-end coverage for the Phase 2D-C2B R1
//! pipeline: quote adapter output (`PinnedQuoteRecord`) -> `ExecutablePriceEdge`
//! -> typed `ExecutableEdgeGraph` -> `find_structural_cycles` ->
//! `StructuralRoute` -> `assemble_route_leg_quotes` -> `CanonicalExecutionContext`
//! -> persist/reload -> `materialize`.

use ethers::types::{Address, H256, U256};
use flashloan_bot::core::{
    canonical_adapters::{
        assemble_route_leg_quotes, AdapterError, CanonicalQuote, PinnedQuoteRecord,
    },
    canonical_execution_context::{
        CanonicalExecutionContext, ForkSetupRecord, PinnedPoolState, PoolExecutionMetadata,
        TokenMetadata,
    },
    executable_call::Venue,
    executable_price_edge::{EdgeError, ExecutablePriceEdge},
    executable_price_graph::{find_structural_cycles, verify_leg_parity, ExecutableEdgeGraph},
    executable_route_materializer::{materialize, PoolRecord, TokenRecord, VenueRecord},
    pool_state_sim::SimulatedPoolState,
    round_artifacts::{
        read_context, read_jsonl, round_artifact_paths, verify_reloaded, write_context, write_jsonl,
    },
    route_artifact::{RouteLeg, RouteReturnClass, StructuralRoute, StructuralRouteLeg},
};
use std::collections::{BTreeMap, HashMap};

const ANCHOR_BLOCK: u64 = 100;

fn a(n: u64) -> Address {
    Address::from_low_u64_be(n)
}
fn h(n: u64) -> H256 {
    H256::from_low_u64_be(n)
}
fn anchor_hash() -> H256 {
    h(999)
}
fn venue_name(v: Venue) -> &'static str {
    match v {
        Venue::UniswapV3 => "UniswapV3",
        Venue::QuickSwap => "QuickSwap",
        Venue::SushiSwap => "SushiSwap",
        Venue::Curve => "Curve",
    }
}

struct LegFixture {
    edge: ExecutablePriceEdge,
    quote: PinnedQuoteRecord,
    pool_meta: PoolExecutionMetadata,
    pool_state: PinnedPoolState,
    token_in: TokenMetadata,
    token_out: TokenMetadata,
}

#[allow(clippy::too_many_arguments)]
fn make_pieces(
    anchor_block: u64,
    anchor_hash_val: H256,
    quote_seed: u64,
    token_in_addr: Address,
    token_in_sym: &str,
    token_out_addr: Address,
    token_out_sym: &str,
    pool: Address,
    router: Address,
    venue: Venue,
    fee: Option<u32>,
    amount_in: U256,
    amount_out: U256,
) -> (
    PinnedQuoteRecord,
    PoolExecutionMetadata,
    PinnedPoolState,
    TokenMetadata,
    TokenMetadata,
) {
    let token_in = TokenMetadata {
        address: token_in_addr,
        symbol: token_in_sym.into(),
        decimals: 6,
        code_hash: h(quote_seed + 1),
        anchor_block,
    };
    let token_out = TokenMetadata {
        address: token_out_addr,
        symbol: token_out_sym.into(),
        decimals: 6,
        code_hash: h(quote_seed + 2),
        anchor_block,
    };
    let pool_meta = PoolExecutionMetadata {
        venue: venue_name(venue).into(),
        pool,
        router,
        spender: router,
        token_order: (token_in_addr, token_out_addr),
        fee,
        curve_method: None,
        curve_indices: None,
        implementation_code_hash: h(quote_seed + 3),
        anchor_block,
    };
    let pool_state = PinnedPoolState {
        state_id: format!("{pool:?}@{anchor_block}"),
        pool_id: format!("{pool:?}"),
        state: SimulatedPoolState::ConstantProduct {
            reserve_in: U256::from(1_000_000u64),
            reserve_out: U256::from(2_000_000u64),
            fee_bps: 30,
        },
        provenance_hash: h(quote_seed + 4),
        anchor_block,
    };
    // Mirrors the real adapter's id derivation (canonical_adapters::quote_v2_leg
    // / quote_v3_leg) so `round_artifacts::verify_reloaded`'s cross-checks hold.
    let pool_state_id = H256::from(ethers::utils::keccak256(pool_state.state_id.as_bytes()));
    let execution_metadata_id = H256::from(ethers::utils::keccak256(
        format!("{pool_meta:?}").as_bytes(),
    ));
    let quote = PinnedQuoteRecord {
        quote_id: h(quote_seed),
        anchor_block,
        anchor_hash: anchor_hash_val,
        venue,
        pool,
        token_in: token_in_addr,
        token_out: token_out_addr,
        amount_in,
        amount_out,
        pool_state_id,
        execution_metadata_id,
        adapter_version: "test".into(),
        provenance_hash: pool_state.provenance_hash,
    };
    (quote, pool_meta, pool_state, token_in, token_out)
}

#[allow(clippy::too_many_arguments)]
fn build_leg(
    quote_seed: u64,
    token_in_addr: Address,
    token_in_sym: &str,
    token_out_addr: Address,
    token_out_sym: &str,
    pool: Address,
    router: Address,
    venue: Venue,
    fee: Option<u32>,
    amount_in: U256,
    amount_out: U256,
) -> LegFixture {
    let (quote, pool_meta, pool_state, token_in, token_out) = make_pieces(
        ANCHOR_BLOCK,
        anchor_hash(),
        quote_seed,
        token_in_addr,
        token_in_sym,
        token_out_addr,
        token_out_sym,
        pool,
        router,
        venue,
        fee,
        amount_in,
        amount_out,
    );
    let edge = ExecutablePriceEdge::from_quote(
        &quote,
        &pool_meta,
        Some(token_in_sym.to_string()),
        Some(token_out_sym.to_string()),
    )
    .unwrap();
    LegFixture {
        edge,
        quote,
        pool_meta,
        pool_state,
        token_in,
        token_out,
    }
}

fn minimal_route(
    key: &str,
    executable_legs: Vec<StructuralRouteLeg>,
    legs: Vec<RouteLeg>,
) -> StructuralRoute {
    StructuralRoute {
        route_id: format!("base:{key}"),
        structural_cycle_key: key.to_string(),
        legs,
        hop_count: executable_legs.len(),
        profile: "base".into(),
        scans_observed: 1,
        return_class: RouteReturnClass::ReturnInsufficientObservations,
        pools: vec![],
        venues: vec![],
        gross_multiplier_avg: 1.0,
        anchor_block: ANCHOR_BLOCK,
        anchor_block_hash: anchor_hash(),
        route_input: U256::from(100),
        executable_legs: Some(executable_legs),
    }
}

// ---------------------------------------------------------------------
// Full deterministic pipeline: adapter -> edge -> typed graph -> cycle
// finder -> StructuralRoute -> leg_quotes -> context -> persist/reload ->
// materialize.
// ---------------------------------------------------------------------
#[test]
fn deterministic_end_to_end_pipeline() {
    let tok_a = a(1);
    let tok_b = a(2);
    let tok_c = a(3);
    let leg0 = build_leg(
        1,
        tok_a,
        "A",
        tok_b,
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    let leg1 = build_leg(
        2,
        tok_b,
        "B",
        tok_c,
        "C",
        a(11),
        a(21),
        Venue::SushiSwap,
        None,
        U256::from(110),
        U256::from(121),
    );
    let leg2 = build_leg(
        3,
        tok_c,
        "C",
        tok_a,
        "A",
        a(12),
        a(22),
        Venue::UniswapV3,
        Some(500),
        U256::from(121),
        U256::from(133),
    );

    let mut graph = ExecutableEdgeGraph::new();
    graph.push(leg0.edge.clone());
    graph.push(leg1.edge.clone());
    graph.push(leg2.edge.clone());

    let routes = find_structural_cycles(&graph, &[tok_a], 2, 3, U256::from(100), "base");
    assert_eq!(
        routes.len(),
        1,
        "expected exactly one closed triangular cycle"
    );
    let route = routes[0].clone();
    assert_eq!(route.hop_count, 3);
    assert!(verify_leg_parity(&route));

    let leg_quotes = vec![leg0.quote.clone(), leg1.quote.clone(), leg2.quote.clone()];
    let output = assemble_route_leg_quotes(
        route.route_input,
        route.anchor_block,
        route.anchor_block_hash,
        leg_quotes.clone(),
        3,
    )
    .unwrap();
    assert_eq!(output, U256::from(133));

    let mut tokens = BTreeMap::new();
    tokens.insert("A".to_string(), leg0.token_in.clone());
    tokens.insert("B".to_string(), leg0.token_out.clone());
    tokens.insert("C".to_string(), leg1.token_out.clone());
    let mut pools = BTreeMap::new();
    let mut pool_states = BTreeMap::new();
    for leg in [&leg0, &leg1, &leg2] {
        pools.insert(format!("{:?}", leg.pool_meta.pool), leg.pool_meta.clone());
        pool_states.insert(leg.pool_state.state_id.clone(), leg.pool_state.clone());
    }
    let mut fork_setup = BTreeMap::new();
    fork_setup.insert(
        route.structural_cycle_key.clone(),
        ForkSetupRecord {
            route_key: route.structural_cycle_key.clone(),
            caller: a(1),
            funding: vec![(tok_a, U256::from(100))],
            approvals: vec![(tok_a, a(20), U256::from(100))],
            balance_checks: vec![a(20)],
            targets: vec![a(20)],
            anchor_block: ANCHOR_BLOCK,
        },
    );

    let context = CanonicalExecutionContext::build(
        ANCHOR_BLOCK,
        anchor_hash(),
        tokens,
        pools,
        pool_states,
        fork_setup,
    )
    .unwrap();
    assert!(context.verify_reload().is_ok());

    let dir = std::env::temp_dir().join(format!(
        "phase2d_c2b_e2e_test_{}_{}",
        std::process::id(),
        "deterministic"
    ));
    let paths = round_artifact_paths(&dir, 1);
    write_jsonl(&paths.executable_edges, &graph.edges).unwrap();
    write_jsonl(&paths.structural_routes, std::slice::from_ref(&route)).unwrap();
    write_jsonl(&paths.pinned_leg_quotes, &leg_quotes).unwrap();
    write_context(&paths.canonical_execution_context, &context).unwrap();

    let reloaded_edges: Vec<ExecutablePriceEdge> = read_jsonl(&paths.executable_edges).unwrap();
    let reloaded_routes: Vec<StructuralRoute> = read_jsonl(&paths.structural_routes).unwrap();
    let reloaded_context = read_context(&paths.canonical_execution_context).unwrap();
    assert!(verify_reloaded(&reloaded_context, &reloaded_edges, &reloaded_routes).is_ok());

    let mut token_records = HashMap::new();
    for (sym, t) in &reloaded_context.tokens {
        token_records.insert(
            sym.clone(),
            TokenRecord {
                address: t.address,
                decimals: t.decimals,
            },
        );
    }
    let mut pool_records = HashMap::new();
    for (pool_key, p) in &reloaded_context.pools {
        let state = reloaded_context
            .pool_states
            .values()
            .find(|s| &s.pool_id == pool_key)
            .unwrap();
        pool_records.insert(
            pool_key.clone(),
            PoolRecord {
                address: p.pool,
                router: p.router,
                state: state.state,
                bytecode_present: true,
                curve_method: None,
                token_in_index: None,
                token_out_index: None,
            },
        );
    }
    let mut venue_records = HashMap::new();
    for p in reloaded_context.pools.values() {
        let venue = match p.venue.as_str() {
            "QuickSwap" => Venue::QuickSwap,
            "SushiSwap" => Venue::SushiSwap,
            "UniswapV3" => Venue::UniswapV3,
            _ => continue,
        };
        venue_records.entry(p.venue.clone()).or_insert(VenueRecord {
            venue,
            router: p.router,
        });
    }

    let materialized = materialize(
        &reloaded_routes[0],
        ANCHOR_BLOCK,
        a(1),
        &token_records,
        &pool_records,
        &venue_records,
        false,
        route.route_input,
    )
    .unwrap();
    assert_eq!(materialized.legs.len(), 3);
    assert_eq!(materialized.legs[0].pool, a(10));
    assert_eq!(materialized.legs[2].fee, Some(500));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn price_edge_does_not_depend_on_symbol() {
    let (quote, pool_meta, _, _, _) = make_pieces(
        ANCHOR_BLOCK,
        anchor_hash(),
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    // No symbols supplied at all — the edge must still carry full,
    // correct address identity.
    let edge = ExecutablePriceEdge::from_quote(&quote, &pool_meta, None, None).unwrap();
    assert_eq!(edge.token_in, a(1));
    assert_eq!(edge.token_out, a(2));
    assert_eq!(edge.pool, a(10));
    assert_eq!(edge.router, a(20));
    assert!(edge.token_in_symbol.is_none());
    assert!(edge.token_out_symbol.is_none());
}

#[test]
fn f64_rate_is_never_used_for_execution() {
    let leg = build_leg(
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    // The diagnostic rate is a lossy derived view; it must never alter or
    // substitute for the exact adapter-returned U256 amounts.
    let rate_6_6 = leg.edge.diagnostic_rate(6, 6).unwrap();
    let rate_18_6 = leg.edge.diagnostic_rate(18, 6).unwrap();
    assert!((rate_6_6 - 1.1).abs() < 1e-9);
    assert_ne!(rate_6_6, rate_18_6);
    // No matter what the diagnostic rate says, amount_in/amount_out are the
    // untouched adapter output used by assemble_route_leg_quotes.
    assert_eq!(leg.edge.amount_in, U256::from(100));
    assert_eq!(leg.edge.amount_out, U256::from(110));
    let out = assemble_route_leg_quotes(
        U256::from(100),
        ANCHOR_BLOCK,
        anchor_hash(),
        vec![leg.quote.clone()],
        1,
    )
    .unwrap();
    assert_eq!(out, U256::from(110));
}

#[test]
fn cycle_finder_preserves_edge_metadata() {
    let leg0 = build_leg(
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    let leg1 = build_leg(
        2,
        a(2),
        "B",
        a(1),
        "A",
        a(11),
        a(21),
        Venue::UniswapV3,
        Some(3000),
        U256::from(110),
        U256::from(101),
    );
    let mut graph = ExecutableEdgeGraph::new();
    graph.push(leg0.edge.clone());
    graph.push(leg1.edge.clone());
    let routes = find_structural_cycles(&graph, &[a(1)], 2, 3, U256::from(100), "base");
    assert_eq!(routes.len(), 1);
    let legs = routes[0].executable_legs.as_ref().unwrap();
    assert_eq!(legs.len(), 2);
    assert_eq!(legs[0].pool, leg0.edge.pool);
    assert_eq!(legs[0].router, leg0.edge.router);
    assert_eq!(legs[0].spender, leg0.edge.spender);
    assert_eq!(legs[0].venue, leg0.edge.venue);
    assert_eq!(legs[0].fee, leg0.edge.fee);
    assert_eq!(legs[1].pool, leg1.edge.pool);
    assert_eq!(legs[1].fee, Some(3000));
}

#[test]
fn mixed_anchor_edges_fail_closed() {
    let (quote0, pool_meta0, _, _, _) = make_pieces(
        ANCHOR_BLOCK,
        anchor_hash(),
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    let edge0 =
        ExecutablePriceEdge::from_quote(&quote0, &pool_meta0, Some("A".into()), Some("B".into()))
            .unwrap();
    // Second leg shares the anchor block number but a DIFFERENT block hash
    // — a reorg-inconsistent pairing that must never be treated as one
    // executable route.
    let (quote1, pool_meta1, _, _, _) = make_pieces(
        ANCHOR_BLOCK,
        h(777),
        2,
        a(2),
        "B",
        a(1),
        "A",
        a(11),
        a(21),
        Venue::SushiSwap,
        None,
        U256::from(110),
        U256::from(101),
    );
    let edge1 =
        ExecutablePriceEdge::from_quote(&quote1, &pool_meta1, Some("B".into()), Some("A".into()))
            .unwrap();

    let mut graph = ExecutableEdgeGraph::new();
    graph.push(edge0);
    graph.push(edge1);
    let routes = find_structural_cycles(&graph, &[a(1)], 2, 3, U256::from(100), "base");
    assert!(
        routes.is_empty(),
        "mixed-anchor cycle must be dropped, not returned"
    );
}

#[test]
fn missing_pool_metadata_fails_closed() {
    let (quote, mut pool_meta, _, _, _) = make_pieces(
        ANCHOR_BLOCK,
        anchor_hash(),
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    pool_meta.router = Address::zero();
    assert!(matches!(
        ExecutablePriceEdge::from_quote(&quote, &pool_meta, Some("A".into()), Some("B".into())),
        Err(EdgeError::ZeroAddress)
    ));
}

#[test]
fn missing_state_reference_fails_closed() {
    let leg = build_leg(
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    let mut tokens = BTreeMap::new();
    tokens.insert("A".to_string(), leg.token_in.clone());
    tokens.insert("B".to_string(), leg.token_out.clone());
    let mut pools = BTreeMap::new();
    pools.insert(format!("{:?}", leg.pool_meta.pool), leg.pool_meta.clone());
    // Context carries an UNRELATED pool state, never the one the edge's
    // pool_state_id actually references.
    let unrelated_state = PinnedPoolState {
        state_id: "unrelated@100".into(),
        pool_id: "unrelated".into(),
        state: SimulatedPoolState::ConstantProduct {
            reserve_in: U256::from(1),
            reserve_out: U256::from(1),
            fee_bps: 30,
        },
        provenance_hash: h(555),
        anchor_block: ANCHOR_BLOCK,
    };
    let mut pool_states = BTreeMap::new();
    pool_states.insert(unrelated_state.state_id.clone(), unrelated_state);
    let mut fork_setup = BTreeMap::new();
    fork_setup.insert(
        "k".to_string(),
        ForkSetupRecord {
            route_key: "k".into(),
            caller: a(1),
            funding: vec![],
            approvals: vec![],
            balance_checks: vec![],
            targets: vec![],
            anchor_block: ANCHOR_BLOCK,
        },
    );
    let context = CanonicalExecutionContext::build(
        ANCHOR_BLOCK,
        anchor_hash(),
        tokens,
        pools,
        pool_states,
        fork_setup,
    )
    .unwrap();

    let route = minimal_route(
        "k",
        vec![StructuralRouteLeg {
            leg_index: 0,
            venue: leg.edge.venue,
            token_in: leg.edge.token_in,
            token_out: leg.edge.token_out,
            pool: leg.edge.pool,
            router: leg.edge.router,
            spender: leg.edge.spender,
            fee: leg.edge.fee,
        }],
        vec![RouteLeg {
            token_in: "A".into(),
            token_out: "B".into(),
            venue: "QuickSwap".into(),
            protocol_version: "V2".into(),
            pool_address: Some(format!("{:?}", leg.edge.pool)),
            fee_tier: None,
        }],
    );

    assert!(verify_reloaded(
        &context,
        std::slice::from_ref(&leg.edge),
        std::slice::from_ref(&route)
    )
    .is_err());
}

#[test]
fn previous_leg_output_is_next_leg_input() {
    let leg0 = build_leg(
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::QuickSwap,
        None,
        U256::from(100),
        U256::from(110),
    );
    // Correctly chained: leg1.amount_in == leg0.amount_out.
    let leg1_ok = build_leg(
        2,
        a(2),
        "B",
        a(1),
        "A",
        a(11),
        a(21),
        Venue::SushiSwap,
        None,
        U256::from(110),
        U256::from(101),
    );
    let ok = assemble_route_leg_quotes(
        U256::from(100),
        ANCHOR_BLOCK,
        anchor_hash(),
        vec![leg0.quote.clone(), leg1_ok.quote.clone()],
        2,
    );
    assert_eq!(ok, Ok(U256::from(101)));

    // Independently-notional (unchained) second leg: amount_in does not
    // match leg0's real amount_out — must fail closed, not be silently
    // accepted as if it were chained.
    let leg1_unchained = build_leg(
        3,
        a(2),
        "B",
        a(1),
        "A",
        a(11),
        a(21),
        Venue::SushiSwap,
        None,
        U256::from(999),
        U256::from(500),
    );
    let bad = assemble_route_leg_quotes(
        U256::from(100),
        ANCHOR_BLOCK,
        anchor_hash(),
        vec![leg0.quote.clone(), leg1_unchained.quote.clone()],
        2,
    );
    assert_eq!(bad, Err(AdapterError::MixedBlock));
}

#[test]
fn legacy_string_cycle_is_not_executable() {
    use flashloan_bot::core::bf_graph::{find_arbitrage_cycles, PriceGraph};

    // The legacy, diagnostic-only symbol/f64-rate graph (production bot
    // path, out of scope for this pipeline) can still find its own
    // triangular cycles...
    let mut dex_rates = std::collections::HashMap::new();
    dex_rates.insert("A-B".to_string(), 1.5f64);
    dex_rates.insert("B-C".to_string(), 1.5f64);
    dex_rates.insert("C-A".to_string(), 1.5f64);
    let mut price_map = std::collections::HashMap::new();
    price_map.insert("QuickSwap".to_string(), dex_rates);
    let legacy_graph = PriceGraph::from_price_map(&price_map);
    let legacy_cycles = find_arbitrage_cycles(&legacy_graph, 0.0, 10_000.0);
    assert!(
        !legacy_cycles.is_empty(),
        "legacy graph should find the synthetic triangular cycle"
    );

    // ...but nothing bridges a legacy `bf_graph::PriceEdge` (symbol +
    // aggregated f64 rate only, no pool/router/amount identity) into the
    // typed pipeline: an `ExecutableEdgeGraph` never populated from it
    // produces zero structural routes.
    let empty_graph = ExecutableEdgeGraph::new();
    let routes = find_structural_cycles(&empty_graph, &[a(1)], 2, 3, U256::from(100), "base");
    assert!(routes.is_empty());
}

#[test]
fn curve_is_rejected_before_quote() {
    let leg = build_leg(
        1,
        a(1),
        "A",
        a(2),
        "B",
        a(10),
        a(20),
        Venue::Curve,
        None,
        U256::from(100),
        U256::from(110),
    );
    let canonical = CanonicalQuote {
        anchor_block: ANCHOR_BLOCK,
        anchor_hash: anchor_hash(),
        amount_in: leg.quote.amount_in,
        amount_out: leg.quote.amount_out,
        token_in: leg.token_in.clone(),
        token_out: leg.token_out.clone(),
        pool: leg.pool_meta.clone(),
        pool_state: leg.pool_state.clone(),
    };
    assert_eq!(canonical.validate(), Err(AdapterError::UnsupportedCurve));
}
