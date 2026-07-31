use ethers::types::{Address, U256};
use flashloan_bot::core::{
    executable_call::{ExecutableCallBuilderRegistry, ExecutionCallContext, Venue},
    executable_route_materializer::{
        materialize, CurveMethod, MaterializationError, PoolRecord, TokenRecord, VenueRecord,
    },
    fresh_economics::{
        FreshEconomicEvaluator, PinnedStateSnapshot, SimulationContext, StatefulRouteEvaluator,
    },
    pool_state_sim::SimulatedPoolState,
    route_artifact::{RouteLeg, RouteReturnClass, StructuralRoute},
};
use std::collections::HashMap;

fn a(n: u64) -> Address {
    Address::from_low_u64_be(n)
}
fn route() -> StructuralRoute {
    StructuralRoute {
        route_id: "base:k".into(),
        structural_cycle_key: "k".into(),
        legs: vec![RouteLeg {
            token_in: format!("{:?}", a(1)),
            token_out: format!("{:?}", a(2)),
            venue: "QuickSwap".into(),
            protocol_version: "V2".into(),
            pool_address: Some(format!("{:?}", a(30))),
            fee_tier: None,
        }],
        hop_count: 1,
        profile: "base".into(),
        scans_observed: 3,
        return_class: RouteReturnClass::ReturnStable,
        pools: vec![format!("{:?}", a(30))],
        venues: vec!["QuickSwap".into()],
        gross_multiplier_avg: 1.1,
        anchor_block: 0,
        anchor_block_hash: ethers::types::H256::zero(),
        route_input: U256::zero(),
        executable_legs: None,
    }
}
fn registries() -> (
    HashMap<String, TokenRecord>,
    HashMap<String, PoolRecord>,
    HashMap<String, VenueRecord>,
) {
    let mut t = HashMap::new();
    t.insert(
        format!("{:?}", a(1)),
        TokenRecord {
            address: a(1),
            decimals: 6,
        },
    );
    t.insert(
        format!("{:?}", a(2)),
        TokenRecord {
            address: a(2),
            decimals: 6,
        },
    );
    let pool = format!("{:?}", a(30));
    let mut p = HashMap::new();
    p.insert(
        pool,
        PoolRecord {
            address: a(30),
            router: a(40),
            state: SimulatedPoolState::ConstantProduct {
                reserve_in: 1_000_000.into(),
                reserve_out: 2_000_000.into(),
                fee_bps: 30,
            },
            bytecode_present: true,
            curve_method: None,
            token_in_index: None,
            token_out_index: None,
        },
    );
    let mut v = HashMap::new();
    v.insert(
        "QuickSwap".into(),
        VenueRecord {
            venue: Venue::QuickSwap,
            router: a(40),
        },
    );
    v.insert(
        "Curve".into(),
        VenueRecord {
            venue: Venue::Curve,
            router: a(40),
        },
    );
    (t, p, v)
}
fn plan() -> flashloan_bot::core::executable_route_materializer::ExecutableRoutePlan {
    let (t, p, v) = registries();
    materialize(&route(), 123, a(99), &t, &p, &v, false, U256::from(1000)).unwrap()
}

#[test]
fn materializer_resolves_real_token_addresses() {
    let p = plan();
    assert_eq!(p.legs[0].token_in, a(1));
    assert_eq!(p.legs[0].token_out, a(2));
}
#[test]
fn materializer_resolves_real_pool_and_router() {
    let p = plan();
    assert_eq!(p.legs[0].pool, a(30));
    assert_eq!(p.legs[0].router, a(40));
}
#[test]
fn materializer_builds_pinned_state_snapshot() {
    assert_eq!(plan().snapshot.pools.len(), 1);
}
#[test]
fn materializer_preserves_route_continuity() {
    let p = plan();
    assert_eq!(p.legs[0].token_in, p.start_token);
}
#[test]
fn materializer_builds_exact_fork_setup() {
    let p = plan();
    assert_eq!(p.fork_setup.anchor_block, 123);
    assert_eq!(p.fork_setup.approvals[0].2, U256::from(1000));
}
#[test]
fn materializer_rejects_missing_curve_indices() {
    let (t, mut p, v) = registries();
    let key = format!("{:?}", a(30));
    p.get_mut(&key).unwrap().curve_method = Some(CurveMethod::Exchange);
    let mut r = route();
    r.legs[0].venue = "Curve".into();
    assert!(matches!(
        materialize(&r, 1, a(9), &t, &p, &v, false, U256::from(1)),
        Err(MaterializationError::InvalidMetadata("Curve metadata"))
    ));
}
#[test]
fn materializer_rejects_missing_pool_state() {
    let (t, mut p, v) = registries();
    let key = format!("{:?}", a(30));
    p.get_mut(&key).unwrap().bytecode_present = false;
    assert!(matches!(
        materialize(&route(), 1, a(9), &t, &p, &v, false, U256::from(1)),
        Err(MaterializationError::InvalidMetadata("pool bytecode"))
    ));
}
#[test]
fn materializer_never_uses_placeholder_metadata() {
    let (t, mut p, v) = registries();
    p.get_mut(&format!("{:?}", a(30))).unwrap().router = Address::zero();
    assert!(materialize(&route(), 1, a(9), &t, &p, &v, false, U256::from(1)).is_err());
}
#[test]
fn rejected_route_never_reaches_materializer() {
    let (t, p, v) = registries();
    assert!(matches!(
        materialize(&route(), 1, a(9), &t, &p, &v, true, U256::from(1)),
        Err(MaterializationError::RejectedRoute)
    ));
}
#[test]
fn materialized_route_is_accepted_by_economics_and_builders() {
    let p = plan();
    let r = route();
    let ev = StatefulRouteEvaluator
        .evaluate(
            &r.legs,
            U256::from(1000),
            &p.snapshot,
            &SimulationContext {
                start_decimals: 6,
                gas_cost_atomic: U256::from(1),
            },
            &[U256::from(1900)],
        )
        .unwrap();
    assert_eq!(ev.final_amount_atomic, U256::from(1900));
    let c = ExecutionCallContext {
        recipient: a(99),
        deadline: U256::from(1),
        amount_out_min: U256::from(1),
        default_sqrt_price_limit_x96: U256::zero(),
        router: Some(a(40)),
        curve_method: None,
        curve_indices: None,
    };
    assert!(ExecutableCallBuilderRegistry::standard()
        .build(&r.legs[0], U256::from(1000), &c)
        .is_ok());
}
