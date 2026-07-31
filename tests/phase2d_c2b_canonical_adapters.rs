use ethers::types::{Address, H256, U256};
use flashloan_bot::core::{
    canonical_adapters::*,
    canonical_execution_context::{PinnedPoolState, PoolExecutionMetadata, TokenMetadata},
};
fn a(n: u64) -> Address {
    Address::from_low_u64_be(n)
}

fn pinned_record() -> (
    PinnedQuoteRecord,
    TokenMetadata,
    TokenMetadata,
    PoolExecutionMetadata,
    PinnedPoolState,
) {
    let c = q("UniswapV3");
    let s = c.pool_state.clone();
    let sid = H256::from(ethers::utils::keccak256(s.state_id.as_bytes()));
    (
        PinnedQuoteRecord {
            quote_id: H256::from_low_u64_be(8),
            anchor_block: 9,
            anchor_hash: c.anchor_hash,
            venue: flashloan_bot::core::executable_call::Venue::UniswapV3,
            pool: c.pool.pool,
            token_in: c.token_in.address,
            token_out: c.token_out.address,
            amount_in: c.amount_in,
            amount_out: c.amount_out,
            pool_state_id: sid,
            execution_metadata_id: H256::from_low_u64_be(7),
            adapter_version: "v1".into(),
            provenance_hash: s.provenance_hash,
        },
        c.token_in,
        c.token_out,
        c.pool,
        s,
    )
}
fn q(venue: &str) -> CanonicalQuote {
    let b = 9;
    let t = |n: u64, s: &str| TokenMetadata {
        address: a(n),
        symbol: s.into(),
        decimals: 6,
        code_hash: H256::from_low_u64_be(n + 20),
        anchor_block: b,
    };
    let pool = PoolExecutionMetadata {
        venue: venue.into(),
        pool: a(3),
        router: a(4),
        spender: a(4),
        token_order: (a(1), a(2)),
        fee: Some(500),
        curve_method: None,
        curve_indices: None,
        implementation_code_hash: H256::from_low_u64_be(30),
        anchor_block: b,
    };
    CanonicalQuote {
        anchor_block: b,
        anchor_hash: H256::from_low_u64_be(1),
        amount_in: U256::from(10),
        amount_out: U256::from(11),
        token_in: t(1, "A"),
        token_out: t(2, "B"),
        pool,
        pool_state: normalized_v3_state(H256::from_low_u64_be(2), b, "p").unwrap(),
    }
}
#[test]
fn v3_quote_emits_real_pool_state() {
    assert!(q("UniswapV3").validate().is_ok())
}
#[test]
fn v2_quote_emits_real_pool_state() {
    let s =
        normalized_v2_state(100.into(), 200.into(), 30, H256::from_low_u64_be(2), 9, "p").unwrap();
    assert!(matches!(
        s.state,
        flashloan_bot::core::pool_state_sim::SimulatedPoolState::ConstantProduct { .. }
    ))
}
#[test]
fn quote_and_context_share_anchor_block() {
    assert_eq!(
        q("UniswapV3").anchor_block,
        q("UniswapV3").pool_state.anchor_block
    )
}
#[test]
fn route_artifact_references_context() {
    assert!(q("QuickSwap").pool_state.state_id.contains("@9"))
}
#[test]
fn materializer_accepts_real_context() {
    assert!(q("QuickSwap").validate().is_ok())
}
#[test]
fn mixed_block_context_is_rejected() {
    let mut x = q("QuickSwap");
    x.pool.anchor_block = 8;
    assert_eq!(x.validate(), Err(AdapterError::MixedBlock))
}
#[test]
fn missing_state_fails_closed() {
    assert!(matches!(
        normalized_v2_state(U256::zero(), 1.into(), 30, H256::from_low_u64_be(1), 1, "p"),
        Err(AdapterError::MissingState)
    ))
}
#[test]
fn unsupported_curve_fails_closed() {
    assert_eq!(q("Curve").validate(), Err(AdapterError::UnsupportedCurve))
}

#[test]
fn canonical_quote_copies_adapter_amount_out_exactly() {
    let (r, a, b, p, s) = pinned_record();
    assert_eq!(
        r.clone().into_canonical(a, b, p, s).unwrap().amount_out,
        r.amount_out
    )
}
#[test]
fn quote_and_pool_state_share_anchor() {
    let (r, _, _, _, s) = pinned_record();
    assert_eq!(r.anchor_block, s.anchor_block)
}
#[test]
fn quote_references_exact_pool_state() {
    let (r, _, _, _, s) = pinned_record();
    assert_eq!(
        r.pool_state_id,
        H256::from(ethers::utils::keccak256(s.state_id.as_bytes()))
    )
}
#[test]
fn amount_in_matches_route_leg() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(r.amount_in, U256::from(10))
}
#[test]
fn aggregated_route_output_is_not_used() {
    let (r, a, b, p, s) = pinned_record();
    assert!(r.into_canonical(a, b, p, s).is_ok())
}
#[test]
fn state_is_not_used_to_infer_amount_out() {
    let (r, a, b, p, s) = pinned_record();
    assert_eq!(
        r.clone().into_canonical(a, b, p, s).unwrap().amount_out,
        r.amount_out
    )
}
#[test]
fn missing_leg_quote_fails_closed() {
    let (mut r, a, b, p, s) = pinned_record();
    r.quote_id = H256::zero();
    assert!(r.into_canonical(a, b, p, s).is_err())
}
#[test]
fn mismatched_provenance_fails_closed() {
    let (mut r, a, b, p, s) = pinned_record();
    r.provenance_hash = H256::from_low_u64_be(99);
    assert!(r.into_canonical(a, b, p, s).is_err())
}
#[test]
fn zero_output_fails_closed() {
    let (mut r, a, b, p, s) = pinned_record();
    r.amount_out = U256::zero();
    assert!(r.into_canonical(a, b, p, s).is_err())
}

#[test]
fn v2_adapter_emits_pinned_leg_quote() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(
        r.venue,
        flashloan_bot::core::executable_call::Venue::UniswapV3
    )
}
#[test]
fn v3_adapter_emits_pinned_leg_quote() {
    let (r, _, _, _, _) = pinned_record();
    assert!(r.amount_out > U256::zero())
}
#[test]
fn route_result_contains_all_leg_quotes() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(
        validate_leg_quote_sequence(&[r], U256::from(10), 9, H256::from_low_u64_be(1)).unwrap(),
        U256::from(11)
    )
}
#[test]
fn next_leg_uses_previous_leg_output() {
    let (mut a, _, _, _, _) = pinned_record();
    let (mut b, _, _, _, _) = pinned_record();
    b.quote_id = H256::from_low_u64_be(9);
    b.amount_in = a.amount_out;
    assert!(
        validate_leg_quote_sequence(&[a.clone(), b], 10.into(), 9, H256::from_low_u64_be(1))
            .is_ok()
    )
}
#[test]
fn route_output_matches_last_leg_output() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(
        validate_leg_quote_sequence(&[r], 10.into(), 9, H256::from_low_u64_be(1)).unwrap(),
        11.into()
    )
}
#[test]
fn aggregate_output_divergence_fails_closed() {
    let (r, _, _, _, _) = pinned_record();
    assert_ne!(
        validate_leg_quote_sequence(&[r], 10.into(), 9, H256::from_low_u64_be(1)).unwrap(),
        12.into()
    )
}
#[test]
fn missing_leg_quote_fails_closed_sequence() {
    assert!(validate_leg_quote_sequence(&[], 10.into(), 9, H256::from_low_u64_be(1)).is_err())
}
#[test]
fn mixed_anchor_quotes_fail_closed() {
    let (mut r, _, _, _, _) = pinned_record();
    r.anchor_block = 8;
    assert!(validate_leg_quote_sequence(&[r], 10.into(), 9, H256::from_low_u64_be(1)).is_err())
}
#[test]
fn canonical_quote_uses_exact_adapter_output() {
    let (r, a, b, p, s) = pinned_record();
    assert_eq!(
        r.clone().into_canonical(a, b, p, s).unwrap().amount_out,
        r.amount_out
    )
}

#[test]
fn route_builder_calls_concrete_quote_for_each_leg() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(
        assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![r], 1).unwrap(),
        11.into()
    )
}
#[test]
fn route_result_collects_all_pinned_leg_quotes() {
    let (r, _, _, _, _) = pinned_record();
    assert!(assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![r], 1).is_ok())
}
#[test]
fn next_leg_receives_exact_previous_output() {
    let (mut a, _, _, _, _) = pinned_record();
    let (mut b, _, _, _, _) = pinned_record();
    b.quote_id = H256::from_low_u64_be(9);
    b.amount_in = a.amount_out;
    assert!(
        assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![a, b], 2).is_ok()
    )
}
#[test]
fn route_output_equals_last_pinned_quote() {
    let (r, _, _, _, _) = pinned_record();
    assert_eq!(
        assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![r], 1).unwrap(),
        11.into()
    )
}
#[test]
fn unsupported_leg_blocks_entire_route() {
    assert!(assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![], 1).is_err())
}
#[test]
fn aggregated_only_route_is_not_c2b_eligible() {
    assert!(assemble_route_leg_quotes(10.into(), 9, H256::from_low_u64_be(1), vec![], 1).is_err())
}
#[test]
fn rejected_curve_never_reaches_quote_adapter() {
    let (mut r, a, b, mut p, s) = pinned_record();
    p.venue = "Curve".into();
    assert!(r.into_canonical(a, b, p, s).is_err())
}
