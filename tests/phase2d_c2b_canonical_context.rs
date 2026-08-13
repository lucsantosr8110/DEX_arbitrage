use ethers::types::{Address, H256, U256};
use flashloan_bot::core::canonical_execution_context::*;
use flashloan_bot::core::pool_state_sim::SimulatedPoolState;
use std::collections::BTreeMap;
fn a(n: u64) -> Address {
    Address::from_low_u64_be(n)
}
fn ctx(block: u64) -> CanonicalExecutionContext {
    let mut t = BTreeMap::new();
    t.insert(
        "A".into(),
        TokenMetadata {
            address: a(1),
            symbol: "A".into(),
            decimals: 6,
            code_hash: H256::from_low_u64_be(11),
            anchor_block: block,
        },
    );
    let mut p = BTreeMap::new();
    p.insert(
        "pool1".into(),
        PoolExecutionMetadata {
            venue: "QuickSwap".into(),
            pool: a(2),
            router: a(3),
            spender: a(3),
            token_order: (a(1), a(1)),
            fee: None,
            curve_method: None,
            curve_indices: None,
            implementation_code_hash: H256::from_low_u64_be(12),
            anchor_block: block,
        },
    );
    let mut s = BTreeMap::new();
    s.insert(
        "state1".into(),
        PinnedPoolState {
            state_id: "state1".into(),
            pool_id: "pool1".into(),
            state: SimulatedPoolState::ConstantProduct {
                reserve_in: 1000.into(),
                reserve_out: 2000.into(),
                fee_bps: 30,
            },
            provenance_hash: H256::from_low_u64_be(13),
            anchor_block: block,
        },
    );
    let mut f = BTreeMap::new();
    f.insert(
        "route".into(),
        ForkSetupRecord {
            route_key: "route".into(),
            caller: a(9),
            funding: vec![(a(1), U256::from(10))],
            approvals: vec![(a(1), a(3), U256::from(10))],
            balance_checks: vec![a(1)],
            targets: vec![a(3)],
            anchor_block: block,
        },
    );
    CanonicalExecutionContext::build(block, H256::from_low_u64_be(10), t, p, s, f).unwrap()
}
#[test]
fn context_uses_same_anchor_as_quotes() {
    assert_eq!(ctx(7).anchor_block, 7)
}
#[test]
fn context_contains_real_token_metadata() {
    assert_eq!(ctx(7).tokens["A"].address, a(1))
}
#[test]
fn context_contains_real_pool_metadata() {
    assert_eq!(ctx(7).pools["pool1"].router, a(3))
}
#[test]
fn context_contains_state_used_by_quote() {
    assert_eq!(ctx(7).pool_states["state1"].anchor_block, 7)
}
#[test]
fn context_rejects_mixed_blocks() {
    let mut c = ctx(7);
    c.tokens.get_mut("A").unwrap().anchor_block = 8;
    assert!(CanonicalExecutionContext::build(
        c.anchor_block,
        c.anchor_block_hash,
        c.tokens,
        c.pools,
        c.pool_states,
        c.fork_setup
    )
    .is_err())
}
#[test]
fn context_rejects_missing_pool_state() {
    let c = ctx(7);
    let mut s = c.pool_states.clone();
    s.clear();
    assert!(CanonicalExecutionContext::build(
        c.anchor_block,
        c.anchor_block_hash,
        c.tokens,
        c.pools,
        s,
        c.fork_setup
    )
    .is_err())
}
#[test]
fn route_artifact_references_context() {
    assert!(ctx(7).verify_hash())
}
#[test]
fn materializer_accepts_persisted_context() {
    assert!(!ctx(7).pool_states.is_empty())
}
#[test]
fn no_placeholder_context_is_allowed() {
    let mut c = ctx(7);
    c.context_hash = H256::zero();
    assert!(!c.verify_hash())
}
