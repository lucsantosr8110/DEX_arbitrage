//! Canonical quote-adapter outputs. Adapters must supply raw on-chain state;
//! no normalized state may be reconstructed from quote amounts.
use crate::core::{
    canonical_execution_context::{PinnedPoolState, PoolExecutionMetadata, TokenMetadata},
    pool_state_sim::SimulatedPoolState,
};
use ethers::types::{Address, H256, U256};
use ethers::{abi::Abi, contract::Contract, providers::Middleware, types::BlockId};
use std::sync::Arc;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct CanonicalQuote {
    pub anchor_block: u64,
    pub anchor_hash: H256,
    pub amount_in: U256,
    pub amount_out: U256,
    pub token_in: TokenMetadata,
    pub token_out: TokenMetadata,
    pub pool: PoolExecutionMetadata,
    pub pool_state: PinnedPoolState,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PinnedQuoteRecord {
    pub quote_id: H256,
    pub anchor_block: u64,
    pub anchor_hash: H256,
    pub venue: crate::core::executable_call::Venue,
    pub pool: Address,
    pub token_in: Address,
    pub token_out: Address,
    pub amount_in: U256,
    pub amount_out: U256,
    pub pool_state_id: H256,
    pub execution_metadata_id: H256,
    pub adapter_version: String,
    pub provenance_hash: H256,
}

pub fn validate_leg_quote_sequence(
    quotes: &[PinnedQuoteRecord],
    route_input: U256,
    anchor_block: u64,
    anchor_hash: H256,
) -> Result<U256, AdapterError> {
    if quotes.is_empty() {
        return Err(AdapterError::MissingState);
    }
    let mut expected = route_input;
    let mut seen = std::collections::HashSet::new();
    for q in quotes {
        if q.anchor_block != anchor_block
            || q.anchor_hash != anchor_hash
            || q.amount_in != expected
            || q.amount_out.is_zero()
            || !seen.insert(q.quote_id)
        {
            return Err(AdapterError::MixedBlock);
        }
        expected = q.amount_out;
    }
    Ok(expected)
}

/// Finalizes a route only from concrete per-leg adapter records. Aggregate
/// route outputs are intentionally not accepted as evidence.
pub fn assemble_route_leg_quotes(
    route_input: U256,
    anchor_block: u64,
    anchor_hash: H256,
    leg_quotes: Vec<PinnedQuoteRecord>,
    expected_legs: usize,
) -> Result<U256, AdapterError> {
    if leg_quotes.len() != expected_legs {
        return Err(AdapterError::MissingState);
    }
    validate_leg_quote_sequence(&leg_quotes, route_input, anchor_block, anchor_hash)
}

impl PinnedQuoteRecord {
    pub fn into_canonical(
        self,
        token_in: TokenMetadata,
        token_out: TokenMetadata,
        pool: PoolExecutionMetadata,
        pool_state: PinnedPoolState,
    ) -> Result<CanonicalQuote, AdapterError> {
        if self.quote_id.is_zero()
            || self.anchor_hash.is_zero()
            || self.amount_in.is_zero()
            || self.amount_out.is_zero()
            || self.pool_state_id.is_zero()
            || self.execution_metadata_id.is_zero()
            || self.provenance_hash.is_zero()
        {
            return Err(AdapterError::IncompleteMetadata);
        }
        if self.anchor_block != token_in.anchor_block
            || self.anchor_block != token_out.anchor_block
            || self.anchor_block != pool.anchor_block
            || self.anchor_block != pool_state.anchor_block
        {
            return Err(AdapterError::MixedBlock);
        }
        if self.pool != pool.pool
            || self.token_in != token_in.address
            || self.token_out != token_out.address
            || self.pool_state_id
                != H256::from(ethers::utils::keccak256(pool_state.state_id.as_bytes()))
        {
            return Err(AdapterError::MixedBlock);
        }
        if self.provenance_hash != pool_state.provenance_hash {
            return Err(AdapterError::MissingState);
        }
        if self.venue != venue_from_name(&pool.venue) || pool.router.is_zero() {
            return Err(AdapterError::IncompleteMetadata);
        }
        Ok(CanonicalQuote {
            anchor_block: self.anchor_block,
            anchor_hash: self.anchor_hash,
            amount_in: self.amount_in,
            amount_out: self.amount_out,
            token_in,
            token_out,
            pool,
            pool_state,
        })
    }
}

fn venue_from_name(name: &str) -> crate::core::executable_call::Venue {
    match name {
        "QuickSwap" => crate::core::executable_call::Venue::QuickSwap,
        "SushiSwap" => crate::core::executable_call::Venue::SushiSwap,
        "UniswapV3" => crate::core::executable_call::Venue::UniswapV3,
        _ => crate::core::executable_call::Venue::Curve,
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AdapterError {
    #[error("ADAPTER_MISSING_STATE")]
    MissingState,
    #[error("ADAPTER_METADATA_INCOMPLETE")]
    IncompleteMetadata,
    #[error("ADAPTER_MIXED_BLOCK")]
    MixedBlock,
    #[error("ADAPTER_UNSUPPORTED_CURVE")]
    UnsupportedCurve,
}

impl CanonicalQuote {
    pub fn validate(&self) -> Result<(), AdapterError> {
        if self.anchor_hash == H256::zero() || self.amount_in.is_zero() || self.amount_out.is_zero()
        {
            return Err(AdapterError::IncompleteMetadata);
        }
        if self.token_in.anchor_block != self.anchor_block
            || self.token_out.anchor_block != self.anchor_block
            || self.pool.anchor_block != self.anchor_block
            || self.pool_state.anchor_block != self.anchor_block
        {
            return Err(AdapterError::MixedBlock);
        }
        if self.pool_state.provenance_hash == H256::zero() {
            return Err(AdapterError::MissingState);
        }
        if self.pool.venue.eq_ignore_ascii_case("Curve") {
            return Err(AdapterError::UnsupportedCurve);
        }
        if self.token_in.address.is_zero()
            || self.token_out.address.is_zero()
            || self.pool.pool.is_zero()
            || self.pool.router.is_zero()
            || self.pool.implementation_code_hash == H256::zero()
        {
            return Err(AdapterError::IncompleteMetadata);
        }
        Ok(())
    }
}

pub fn normalized_v2_state(
    reserve_in: U256,
    reserve_out: U256,
    fee_bps: u32,
    provenance: H256,
    anchor_block: u64,
    pool_id: &str,
) -> Result<PinnedPoolState, AdapterError> {
    if reserve_in.is_zero() || reserve_out.is_zero() || provenance == H256::zero() {
        return Err(AdapterError::MissingState);
    }
    Ok(PinnedPoolState {
        state_id: format!("{pool_id}@{anchor_block}"),
        pool_id: pool_id.into(),
        state: SimulatedPoolState::ConstantProduct {
            reserve_in,
            reserve_out,
            fee_bps,
        },
        provenance_hash: provenance,
        anchor_block,
    })
}

pub fn normalized_v3_state(
    provenance: H256,
    anchor_block: u64,
    pool_id: &str,
) -> Result<PinnedPoolState, AdapterError> {
    if provenance == H256::zero() {
        return Err(AdapterError::MissingState);
    }
    Ok(PinnedPoolState {
        state_id: format!("{pool_id}@{anchor_block}"),
        pool_id: pool_id.into(),
        state: SimulatedPoolState::Opaque(
            crate::core::pool_state_sim::PoolKind::UniswapV3Concentrated,
        ),
        provenance_hash: provenance,
        anchor_block,
    })
}

pub fn code_hash(code: &[u8]) -> Result<H256, AdapterError> {
    if code.is_empty() {
        Err(AdapterError::IncompleteMetadata)
    } else {
        Ok(H256::from(ethers::utils::keccak256(code)))
    }
}

#[derive(Debug, Clone)]
pub struct OnlinePoolRead {
    pub anchor_block: u64,
    pub token0: Address,
    pub token1: Address,
    pub reserve0: Option<U256>,
    pub reserve1: Option<U256>,
    pub sqrt_price_x96: Option<U256>,
    pub liquidity: Option<U256>,
    pub fee: Option<u32>,
    pub pool_code_hash: H256,
    pub router_code_hash: H256,
}

const V2_READ_ABI: &str = r#"[{"name":"getReserves","outputs":[{"name":"reserve0","type":"uint112"},{"name":"reserve1","type":"uint112"},{"name":"blockTimestampLast","type":"uint32"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"token0","outputs":[{"name":"","type":"address"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"token1","outputs":[{"name":"","type":"address"}],"inputs":[],"stateMutability":"view","type":"function"}]"#;
const V3_READ_ABI: &str = r#"[{"name":"slot0","outputs":[{"name":"sqrtPriceX96","type":"uint160"},{"name":"tick","type":"int24"},{"name":"observationIndex","type":"uint16"},{"name":"observationCardinality","type":"uint16"},{"name":"observationCardinalityNext","type":"uint16"},{"name":"feeProtocol","type":"uint8"},{"name":"unlocked","type":"bool"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"liquidity","outputs":[{"name":"","type":"uint128"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"token0","outputs":[{"name":"","type":"address"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"token1","outputs":[{"name":"","type":"address"}],"inputs":[],"stateMutability":"view","type":"function"},{"name":"fee","outputs":[{"name":"","type":"uint24"}],"inputs":[],"stateMutability":"view","type":"function"}]"#;

pub async fn read_v2_pool<M: Middleware>(
    provider: Arc<M>,
    pool: Address,
    router: Address,
    anchor_block: u64,
) -> Result<OnlinePoolRead, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V2_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(pool, abi, provider.clone());
    let b = BlockId::Number(anchor_block.into());
    let reserves: (U256, U256, U256) = c
        .method::<_, (U256, U256, U256)>("getReserves", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let token0: Address = c
        .method::<_, Address>("token0", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let token1: Address = c
        .method::<_, Address>("token1", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let pc = provider
        .get_code(pool, Some(b))
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let rc = provider
        .get_code(router, Some(b))
        .await
        .map_err(|_| AdapterError::MissingState)?;
    Ok(OnlinePoolRead {
        anchor_block,
        token0,
        token1,
        reserve0: Some(reserves.0),
        reserve1: Some(reserves.1),
        sqrt_price_x96: None,
        liquidity: None,
        fee: None,
        pool_code_hash: code_hash(&pc.0)?,
        router_code_hash: code_hash(&rc.0)?,
    })
}

pub async fn read_v3_pool<M: Middleware>(
    provider: Arc<M>,
    pool: Address,
    router: Address,
    anchor_block: u64,
) -> Result<OnlinePoolRead, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V3_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(pool, abi, provider.clone());
    let b = BlockId::Number(anchor_block.into());
    let slot: (U256, i32, u16, u16, u16, u8, bool) = c
        .method::<_, (U256, i32, u16, u16, u16, u8, bool)>("slot0", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let liq: U256 = c
        .method::<_, U256>("liquidity", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let token0: Address = c
        .method::<_, Address>("token0", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let token1: Address = c
        .method::<_, Address>("token1", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let fee: U256 = c
        .method::<_, U256>("fee", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let pc = provider
        .get_code(pool, Some(b))
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let rc = provider
        .get_code(router, Some(b))
        .await
        .map_err(|_| AdapterError::MissingState)?;
    Ok(OnlinePoolRead {
        anchor_block,
        token0,
        token1,
        reserve0: None,
        reserve1: None,
        sqrt_price_x96: Some(slot.0),
        liquidity: Some(liq),
        fee: Some(fee.as_u32()),
        pool_code_hash: code_hash(&pc.0)?,
        router_code_hash: code_hash(&rc.0)?,
    })
}

const V2_FACTORY_READ_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenA","type":"address"},{"internalType":"address","name":"tokenB","type":"address"}],"name":"getPair","outputs":[{"internalType":"address","name":"pair","type":"address"}],"stateMutability":"view","type":"function"}]"#;
const V3_FACTORY_READ_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenA","type":"address"},{"internalType":"address","name":"tokenB","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"}],"name":"getPool","outputs":[{"internalType":"address","name":"pool","type":"address"}],"stateMutability":"view","type":"function"}]"#;

/// Resolves a V2 pair address via the venue's factory, pinned to
/// `anchor_block`. Returns `None` (not an error) when no pair exists for
/// this token combination at this venue.
pub async fn resolve_v2_pool_address<M: Middleware>(
    provider: Arc<M>,
    factory: Address,
    token_a: Address,
    token_b: Address,
    anchor_block: u64,
) -> Result<Option<Address>, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V2_FACTORY_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(factory, abi, provider);
    let pair: Address = c
        .method::<_, Address>("getPair", (token_a, token_b))
        .map_err(|_| AdapterError::MissingState)?
        .block(BlockId::Number(anchor_block.into()))
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    Ok((!pair.is_zero()).then_some(pair))
}

/// Resolves a V3 pool address for a given fee tier via the venue's factory,
/// pinned to `anchor_block`. Returns `None` (not an error) when no pool
/// exists for this token/fee combination.
pub async fn resolve_v3_pool_address<M: Middleware>(
    provider: Arc<M>,
    factory: Address,
    token_a: Address,
    token_b: Address,
    fee: u32,
    anchor_block: u64,
) -> Result<Option<Address>, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V3_FACTORY_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(factory, abi, provider);
    let pool: Address = c
        .method::<_, Address>("getPool", (token_a, token_b, fee))
        .map_err(|_| AdapterError::MissingState)?
        .block(BlockId::Number(anchor_block.into()))
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    Ok((!pool.is_zero()).then_some(pool))
}

#[allow(clippy::too_many_arguments)]
pub async fn quote_v2_leg<M: Middleware>(
    provider: Arc<M>,
    venue: crate::core::executable_call::Venue,
    router: Address,
    pool: Address,
    token_in: Address,
    token_out: Address,
    amount_in: U256,
    anchor_block: u64,
    anchor_hash: H256,
    token_meta_in: TokenMetadata,
    token_meta_out: TokenMetadata,
    pool_meta: PoolExecutionMetadata,
    pool_state: PinnedPoolState,
) -> Result<(U256, PinnedQuoteRecord), AdapterError> {
    if token_meta_in.address != token_in
        || token_meta_out.address != token_out
        || token_meta_in.anchor_block != anchor_block
        || token_meta_out.anchor_block != anchor_block
        || pool_meta.anchor_block != anchor_block
    {
        return Err(AdapterError::MixedBlock);
    }
    let abi: Abi = serde_json::from_str(r#"[{"name":"getAmountsOut","outputs":[{"name":"amounts","type":"uint256[]"}],"inputs":[{"name":"amountIn","type":"uint256"},{"name":"path","type":"address[]"}],"stateMutability":"view","type":"function"}]"#).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(router, abi, provider);
    let amounts: Vec<U256> = c
        .method::<_, Vec<U256>>("getAmountsOut", (amount_in, vec![token_in, token_out]))
        .map_err(|_| AdapterError::MissingState)?
        .block(BlockId::Number(anchor_block.into()))
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    let out = *amounts.last().ok_or(AdapterError::MissingState)?;
    if out.is_zero() {
        return Err(AdapterError::IncompleteMetadata);
    }
    let state_id = H256::from(ethers::utils::keccak256(pool_state.state_id.as_bytes()));
    let meta_id = H256::from(ethers::utils::keccak256(
        format!("{:?}", pool_meta).as_bytes(),
    ));
    let quote_id = H256::from(ethers::utils::keccak256(
        format!("v2:{pool:?}:{anchor_block}:{amount_in}").as_bytes(),
    ));
    Ok((
        out,
        PinnedQuoteRecord {
            quote_id,
            anchor_block,
            anchor_hash,
            venue,
            pool,
            token_in,
            token_out,
            amount_in,
            amount_out: out,
            pool_state_id: state_id,
            execution_metadata_id: meta_id,
            adapter_version: "v2-router-getAmountsOut".into(),
            provenance_hash: pool_state.provenance_hash,
        },
    ))
}

pub async fn quote_v3_leg<M: Middleware>(
    provider: Arc<M>,
    quoter: Address,
    pool: Address,
    token_in: Address,
    token_out: Address,
    fee: u32,
    amount_in: U256,
    anchor_block: u64,
    anchor_hash: H256,
    token_meta_in: TokenMetadata,
    token_meta_out: TokenMetadata,
    pool_meta: PoolExecutionMetadata,
    pool_state: PinnedPoolState,
) -> Result<(U256, PinnedQuoteRecord), AdapterError> {
    if token_meta_in.address != token_in
        || token_meta_out.address != token_out
        || token_meta_in.anchor_block != anchor_block
        || token_meta_out.anchor_block != anchor_block
        || pool_meta.anchor_block != anchor_block
    {
        return Err(AdapterError::MixedBlock);
    }
    let abi: Abi = serde_json::from_str(r#"[{"name":"quoteExactInputSingle","outputs":[{"name":"amountOut","type":"uint256"}],"inputs":[{"name":"tokenIn","type":"address"},{"name":"tokenOut","type":"address"},{"name":"fee","type":"uint24"},{"name":"amountIn","type":"uint256"},{"name":"sqrtPriceLimitX96","type":"uint160"}],"stateMutability":"nonpayable","type":"function"}]"#).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(quoter, abi, provider);
    let out: U256 = c
        .method::<_, U256>(
            "quoteExactInputSingle",
            (token_in, token_out, fee, amount_in, U256::zero()),
        )
        .map_err(|_| AdapterError::MissingState)?
        .block(BlockId::Number(anchor_block.into()))
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    if out.is_zero() {
        return Err(AdapterError::IncompleteMetadata);
    }
    let state_id = H256::from(ethers::utils::keccak256(pool_state.state_id.as_bytes()));
    let meta_id = H256::from(ethers::utils::keccak256(
        format!("{:?}", pool_meta).as_bytes(),
    ));
    let quote_id = H256::from(ethers::utils::keccak256(
        format!("v3:{pool:?}:{anchor_block}:{amount_in}").as_bytes(),
    ));
    Ok((
        out,
        PinnedQuoteRecord {
            quote_id,
            anchor_block,
            anchor_hash,
            venue: crate::core::executable_call::Venue::UniswapV3,
            pool,
            token_in,
            token_out,
            amount_in,
            amount_out: out,
            pool_state_id: state_id,
            execution_metadata_id: meta_id,
            adapter_version: "v3-quoter-quoteExactInputSingle".into(),
            provenance_hash: pool_state.provenance_hash,
        },
    ))
}

#[allow(dead_code)]
fn _address(_: Address) {}
