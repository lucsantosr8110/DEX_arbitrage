use ethers::types::{Address, U256};
use flashloan_bot::core::{executable_call::*, route_artifact::RouteLeg};

fn leg(venue: &str) -> RouteLeg {
    RouteLeg {
        token_in: "0x0000000000000000000000000000000000000001".into(),
        token_out: "0x0000000000000000000000000000000000000002".into(),
        venue: venue.into(),
        protocol_version: if venue == "UniswapV3" {
            "V3".into()
        } else {
            "V2".into()
        },
        pool_address: Some("0x0000000000000000000000000000000000000003".into()),
        fee_tier: Some(500),
    }
}
fn ctx() -> ExecutionCallContext {
    ExecutionCallContext {
        recipient: Address::from_low_u64_be(9),
        deadline: U256::from(100),
        amount_out_min: U256::from(7),
        default_sqrt_price_limit_x96: U256::zero(),
        router: Some(Address::from_low_u64_be(8)),
        curve_method: None,
        curve_indices: None,
    }
}

#[test]
fn uniswap_v3_exact_input_single_selector_and_tuple() {
    let call = ExecutableCallBuilderRegistry::standard()
        .build(&leg("UniswapV3"), U256::from(10), &ctx())
        .unwrap();
    assert_eq!(hex::encode(call.selector), "414bf389");
    assert_eq!(&call.calldata[..4], &call.selector);
    assert_eq!(call.approvals[0].amount, U256::from(10));
}

#[test]
fn v2_builder_encodes_real_swap_and_zero_value() {
    let call = ExecutableCallBuilderRegistry::standard()
        .build(&leg("QuickSwap"), U256::from(10), &ctx())
        .unwrap();
    assert_eq!(call.method, "swapExactTokensForTokens");
    assert!(call.value.is_zero());
    assert_eq!(&call.calldata[..4], &call.selector);
}

#[test]
fn builders_reject_zero_amount_and_unknown_curve_metadata() {
    assert!(matches!(
        ExecutableCallBuilderRegistry::standard().build(&leg("QuickSwap"), U256::zero(), &ctx()),
        Err(ExecutableCallError::InvalidAmount)
    ));
    assert!(matches!(
        ExecutableCallBuilderRegistry::standard().build(&leg("Curve"), U256::from(1), &ctx()),
        Err(ExecutableCallError::InvalidCurveIndices)
    ));
}

#[test]
fn output_of_first_leg_is_amount_of_second_leg() {
    let registry = ExecutableCallBuilderRegistry::standard();
    let first = registry
        .build(&leg("QuickSwap"), U256::from(111), &ctx())
        .unwrap();
    let second = registry
        .build(&leg("QuickSwap"), U256::from(222), &ctx())
        .unwrap();
    assert_ne!(first.calldata, second.calldata);
}
