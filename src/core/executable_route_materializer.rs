//! Canonical conversion of structural routes into executable route plans.
//! This layer is pure: it performs no RPC calls and never invents metadata.

use crate::core::{
    executable_call::Venue, fresh_economics::PinnedStateSnapshot,
    pool_state_sim::SimulatedPoolState, route_artifact::StructuralRoute,
};
use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::str::FromStr;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CurveMethod {
    Exchange,
    ExchangeUnderlying,
}

#[derive(Debug, Clone)]
pub struct TokenRecord {
    pub address: Address,
    pub decimals: u8,
}

#[derive(Debug, Clone)]
pub struct PoolRecord {
    pub address: Address,
    pub router: Address,
    pub state: SimulatedPoolState,
    pub bytecode_present: bool,
    pub curve_method: Option<CurveMethod>,
    pub token_in_index: Option<i128>,
    pub token_out_index: Option<i128>,
}

#[derive(Debug, Clone)]
pub struct VenueRecord {
    pub venue: Venue,
    pub router: Address,
}

#[derive(Debug, Clone)]
pub struct ForkSetupPlan {
    pub anchor_block: u64,
    pub caller: Address,
    pub tokens: Vec<Address>,
    pub funding: Vec<(Address, U256)>,
    pub approvals: Vec<(Address, Address, U256)>,
    pub targets: Vec<Address>,
    pub balance_checks: Vec<Address>,
}

#[derive(Debug, Clone)]
pub struct ExecutableLegPlan {
    pub venue: Venue,
    pub token_in: Address,
    pub token_out: Address,
    pub pool: Address,
    pub router: Address,
    pub fee: Option<u32>,
    pub curve_method: Option<CurveMethod>,
    pub token_in_index: Option<i128>,
    pub token_out_index: Option<i128>,
    pub spender: Address,
}

#[derive(Debug, Clone)]
pub struct ExecutableRoutePlan {
    pub structural_cycle_key: String,
    pub anchor_block: u64,
    pub start_token: Address,
    pub legs: Vec<ExecutableLegPlan>,
    pub snapshot: PinnedStateSnapshot,
    pub fork_setup: ForkSetupPlan,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum MaterializationError {
    #[error("REJECTED_ROUTE")]
    RejectedRoute,
    #[error("MATERIALIZATION_MISSING_TOKEN: {0}")]
    MissingToken(String),
    #[error("MATERIALIZATION_MISSING_POOL")]
    MissingPool,
    #[error("MATERIALIZATION_MISSING_ROUTER")]
    MissingRouter,
    #[error("MATERIALIZATION_UNSUPPORTED_POOL_STATE")]
    UnsupportedPoolState,
    #[error("MATERIALIZATION_INVALID_METADATA: {0}")]
    InvalidMetadata(&'static str),
    #[error("MATERIALIZATION_TOKEN_DISCONTINUITY")]
    TokenDiscontinuity,
    #[error("MATERIALIZATION_MISSING_TYPED_LEGS")]
    MissingTypedLegs,
}

/// Canonical materialization entry point.  Unlike the compatibility
/// `materialize` below, it never reads `RouteLeg::token_in` or
/// `RouteLeg::token_out`: those fields are presentation strings and must not
/// participate in executable identity.
pub fn materialize_canonical(
    route: &StructuralRoute,
    anchor_block: u64,
    caller: Address,
    pools: &HashMap<String, PoolRecord>,
    rejected: bool,
    amount_in: U256,
) -> Result<ExecutableRoutePlan, MaterializationError> {
    if rejected {
        return Err(MaterializationError::RejectedRoute);
    }
    if amount_in.is_zero() {
        return Err(MaterializationError::InvalidMetadata("amount"));
    }
    let typed_legs = route
        .executable_legs
        .as_ref()
        .ok_or(MaterializationError::MissingTypedLegs)?;
    if typed_legs.is_empty() {
        return Err(MaterializationError::MissingTypedLegs);
    }

    let mut snapshot = PinnedStateSnapshot::default();
    let mut legs: Vec<ExecutableLegPlan> = Vec::with_capacity(typed_legs.len());
    let mut token_addresses = Vec::with_capacity(typed_legs.len() * 2);
    let mut approvals = Vec::with_capacity(typed_legs.len());
    let mut targets = Vec::with_capacity(typed_legs.len());
    let mut funding = Vec::with_capacity(typed_legs.len());

    for (index, typed) in typed_legs.iter().enumerate() {
        if typed.token_in.is_zero()
            || typed.token_out.is_zero()
            || typed.token_in == typed.token_out
        {
            return Err(MaterializationError::InvalidMetadata("typed token"));
        }
        if index > 0 && legs[index - 1].token_out != typed.token_in {
            return Err(MaterializationError::TokenDiscontinuity);
        }
        let key = format!("{:?}", typed.pool);
        let pool = pools.get(&key).ok_or(MaterializationError::MissingPool)?;
        if pool.address != typed.pool || pool.router != typed.router || !pool.bytecode_present {
            return Err(MaterializationError::InvalidMetadata("typed pool metadata"));
        }
        if typed.router.is_zero() || typed.spender.is_zero() {
            return Err(MaterializationError::MissingRouter);
        }
        if typed.venue == Venue::UniswapV3 && typed.fee.is_none() {
            return Err(MaterializationError::InvalidMetadata("V3 fee"));
        }
        if typed.venue == Venue::Curve {
            return Err(MaterializationError::InvalidMetadata("Curve metadata"));
        }
        snapshot.pools.insert(key, pool.state);
        legs.push(ExecutableLegPlan {
            venue: typed.venue,
            token_in: typed.token_in,
            token_out: typed.token_out,
            pool: typed.pool,
            router: typed.router,
            fee: typed.fee,
            curve_method: None,
            token_in_index: None,
            token_out_index: None,
            spender: typed.spender,
        });
        approvals.push((typed.token_in, typed.spender, amount_in));
        funding.push((typed.token_in, amount_in));
        targets.push(typed.router);
        token_addresses.extend([typed.token_in, typed.token_out]);
    }
    token_addresses.sort();
    token_addresses.dedup();
    targets.sort();
    targets.dedup();
    Ok(ExecutableRoutePlan {
        structural_cycle_key: route.structural_cycle_key.clone(),
        anchor_block,
        start_token: legs[0].token_in,
        legs,
        snapshot,
        fork_setup: ForkSetupPlan {
            anchor_block,
            caller,
            tokens: token_addresses,
            funding,
            approvals,
            targets: targets.clone(),
            balance_checks: targets,
        },
    })
}

pub fn materialize(
    route: &StructuralRoute,
    anchor_block: u64,
    caller: Address,
    tokens: &HashMap<String, TokenRecord>,
    pools: &HashMap<String, PoolRecord>,
    venues: &HashMap<String, VenueRecord>,
    rejected: bool,
    amount_in: U256,
) -> Result<ExecutableRoutePlan, MaterializationError> {
    if rejected {
        return Err(MaterializationError::RejectedRoute);
    }
    if route.legs.is_empty() || amount_in.is_zero() {
        return Err(MaterializationError::InvalidMetadata(
            "empty route or amount",
        ));
    }
    let mut snapshot = PinnedStateSnapshot::default();
    let mut legs: Vec<ExecutableLegPlan> = Vec::with_capacity(route.legs.len());
    let mut token_addresses = Vec::new();
    let mut approvals = Vec::new();
    let mut targets = Vec::new();
    let mut funding = Vec::new();
    for (i, leg) in route.legs.iter().enumerate() {
        let tin = tokens
            .get(&leg.token_in)
            .ok_or_else(|| MaterializationError::MissingToken(leg.token_in.clone()))?;
        let tout = tokens
            .get(&leg.token_out)
            .ok_or_else(|| MaterializationError::MissingToken(leg.token_out.clone()))?;
        if tin.decimals == 0 || tout.decimals == 0 {
            return Err(MaterializationError::InvalidMetadata("decimals"));
        }
        if i > 0 && legs[i - 1].token_out != tin.address {
            return Err(MaterializationError::TokenDiscontinuity);
        }
        let pool_text = leg
            .pool_address
            .as_ref()
            .ok_or(MaterializationError::MissingPool)?;
        let pool_addr =
            Address::from_str(pool_text).map_err(|_| MaterializationError::MissingPool)?;
        let pool = pools
            .get(pool_text)
            .ok_or(MaterializationError::MissingPool)?;
        if pool.address != pool_addr || !pool.bytecode_present {
            return Err(MaterializationError::InvalidMetadata("pool bytecode"));
        }
        let venue = venues
            .get(&leg.venue)
            .ok_or(MaterializationError::MissingRouter)?;
        if venue.router.is_zero() || pool.router.is_zero() || venue.router != pool.router {
            return Err(MaterializationError::MissingRouter);
        }
        if leg.protocol_version.eq_ignore_ascii_case("V3") && leg.fee_tier.is_none() {
            return Err(MaterializationError::InvalidMetadata("V3 fee"));
        }
        if leg.venue.eq_ignore_ascii_case("Curve")
            && (pool.curve_method.is_none()
                || pool.token_in_index.is_none()
                || pool.token_out_index.is_none())
        {
            return Err(MaterializationError::InvalidMetadata("Curve metadata"));
        }
        let id = crate::core::route_artifact::pool_identity(leg);
        snapshot.pools.insert(id, pool.state);
        let plan = ExecutableLegPlan {
            venue: venue.venue,
            token_in: tin.address,
            token_out: tout.address,
            pool: pool.address,
            router: pool.router,
            fee: leg.fee_tier,
            curve_method: pool.curve_method.clone(),
            token_in_index: pool.token_in_index,
            token_out_index: pool.token_out_index,
            spender: pool.router,
        };
        approvals.push((tin.address, pool.router, amount_in));
        funding.push((tin.address, amount_in));
        targets.push(pool.router);
        token_addresses.extend([tin.address, tout.address]);
        legs.push(plan);
    }
    token_addresses.sort();
    token_addresses.dedup();
    targets.sort();
    targets.dedup();
    Ok(ExecutableRoutePlan {
        structural_cycle_key: route.structural_cycle_key.clone(),
        anchor_block,
        start_token: legs[0].token_in,
        legs,
        snapshot,
        fork_setup: ForkSetupPlan {
            anchor_block,
            caller,
            tokens: token_addresses,
            funding,
            approvals,
            targets: targets.clone(),
            balance_checks: targets,
        },
    })
}
