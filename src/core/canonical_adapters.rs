//! Canonical quote-adapter outputs. Adapters must supply raw on-chain state;
//! no normalized state may be reconstructed from quote amounts.
use crate::core::{
    canonical_execution_context::{PinnedPoolState, PoolExecutionMetadata, TokenMetadata},
    canonical_metadata_cache::{CodeKey, PoolMetadataKey, Resolved, V2PairKey, V3PoolKey},
    pool_state_sim::SimulatedPoolState,
};
use ethers::types::{Address, H256, U256};
use ethers::{abi::Abi, contract::Contract, providers::Middleware, types::BlockId};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use thiserror::Error;

/// One measured RPC round-trip made while resolving a pool address, reading
/// pool state, or pulling a quote leg. Collected only where a caller passes
/// a live `QuoteMetrics` (Phase-A single-leg quoting); `QuoteMetrics::disabled()`
/// call sites (Phase-B re-quotes) record nothing, so this never grows across
/// a re-quote pass that already has its own `(pool, amount_in)` cache.
#[derive(Debug, Clone, Copy)]
pub struct RpcCallRecord {
    pub call_kind: &'static str,
    pub pool: Address,
    pub wait_ms: u64,
}

/// Diagnostic RPC-call collector threaded through the Phase-A adapter
/// functions so `quote_ms` can be decomposed into real eth_call wait time,
/// call count, and duplicate reads after the round completes. `disabled()`
/// is a zero-cost no-op sink for call sites outside that decomposition.
#[derive(Clone, Default)]
pub struct QuoteMetrics(Option<Arc<Mutex<Vec<RpcCallRecord>>>>);

impl QuoteMetrics {
    pub fn new() -> Self {
        Self(Some(Arc::new(Mutex::new(Vec::new()))))
    }

    pub fn disabled() -> Self {
        Self(None)
    }

    fn record(&self, call_kind: &'static str, pool: Address, wait: Duration) {
        if let Some(sink) = &self.0 {
            sink.lock().unwrap().push(RpcCallRecord {
                call_kind,
                pool,
                wait_ms: wait.as_millis() as u64,
            });
        }
    }

    pub fn drain(&self) -> Vec<RpcCallRecord> {
        match &self.0 {
            Some(sink) => std::mem::take(&mut *sink.lock().unwrap()),
            None => Vec::new(),
        }
    }
}

/// Process-lifetime cache of Phase-A identity lookups (pair/pool address
/// resolution, token0/token1, contract bytecode hash) -- never pool state,
/// never a quote amount. Owned by `CanonicalDiscoveryService`, so it
/// persists across rounds.
pub type MetadataCache =
    crate::core::canonical_metadata_cache::CanonicalMetadataCache<Address, H256>;

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

/// Fetches `token0`/`token1` for `pool`, single-flighted and cached
/// forever (a deployed pool's token order never changes) keyed on
/// `(chain_id, pool)`. Real RPC only ever happens on the first lookup
/// process-wide; every later call (same round, later round, reverse-
/// direction pair) is a cache hit or a suppressed single-flight waiter.
async fn cached_pool_tokens<M: Middleware>(
    provider: &Arc<M>,
    pool: Address,
    abi: &Abi,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<(Address, Address), AdapterError> {
    let b = BlockId::Number(anchor_block.into());
    let key = PoolMetadataKey { chain_id, pool };
    let token0 = {
        let provider = provider.clone();
        let abi = abi.clone();
        cache
            .token0(key, anchor_block, || async move {
                let c = Contract::new(pool, abi, provider);
                let t0 = Instant::now();
                let v: Address = c
                    .method::<_, Address>("token0", ())
                    .map_err(|_| AdapterError::MissingState)?
                    .block(b)
                    .call()
                    .await
                    .map_err(|_| AdapterError::MissingState)?;
                metrics.record("token0", pool, t0.elapsed());
                Ok(Resolved::Positive(v))
            })
            .await?
            .ok_or(AdapterError::MissingState)?
    };
    let token1 = {
        let provider = provider.clone();
        let abi = abi.clone();
        cache
            .token1(key, anchor_block, || async move {
                let c = Contract::new(pool, abi, provider);
                let t0 = Instant::now();
                let v: Address = c
                    .method::<_, Address>("token1", ())
                    .map_err(|_| AdapterError::MissingState)?
                    .block(b)
                    .call()
                    .await
                    .map_err(|_| AdapterError::MissingState)?;
                metrics.record("token1", pool, t0.elapsed());
                Ok(Resolved::Positive(v))
            })
            .await?
            .ok_or(AdapterError::MissingState)?
    };
    Ok((token0, token1))
}

/// Fetches the bytecode hash of `address`, single-flighted and cached
/// keyed on `(chain_id, address)`. Empty bytecode is a negative
/// resolution -- bounded-TTL, since an address can gain code later (a
/// factory-returned pool address should already have code, but this
/// stays defensive rather than assuming). Only the hash is retained, not
/// the raw bytecode -- nothing downstream needs the bytes themselves.
async fn cached_code_hash<M: Middleware>(
    provider: &Arc<M>,
    address: Address,
    call_kind: &'static str,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<H256, AdapterError> {
    let b = BlockId::Number(anchor_block.into());
    let provider = provider.clone();
    cache
        .code_hash(CodeKey { chain_id, address }, anchor_block, || async move {
            let t0 = Instant::now();
            let code = provider
                .get_code(address, Some(b))
                .await
                .map_err(|_| AdapterError::MissingState)?;
            metrics.record(call_kind, address, t0.elapsed());
            if code.0.is_empty() {
                Ok(Resolved::Negative {
                    observed_at_block: anchor_block,
                })
            } else {
                Ok(Resolved::Positive(code_hash(&code.0)?))
            }
        })
        .await?
        .ok_or(AdapterError::MissingState)
}

#[allow(clippy::too_many_arguments)]
pub async fn read_v2_pool<M: Middleware>(
    provider: Arc<M>,
    pool: Address,
    router: Address,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<OnlinePoolRead, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V2_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(pool, abi.clone(), provider.clone());
    let b = BlockId::Number(anchor_block.into());
    let t0 = Instant::now();
    let reserves: (U256, U256, U256) = c
        .method::<_, (U256, U256, U256)>("getReserves", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record("getReserves", pool, t0.elapsed());
    let (token0, token1) = cached_pool_tokens(
        &provider,
        pool,
        &abi,
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let pool_code_hash = cached_code_hash(
        &provider,
        pool,
        "get_code(pool)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let router_code_hash = cached_code_hash(
        &provider,
        router,
        "get_code(router)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    Ok(OnlinePoolRead {
        anchor_block,
        token0,
        token1,
        reserve0: Some(reserves.0),
        reserve1: Some(reserves.1),
        sqrt_price_x96: None,
        liquidity: None,
        fee: None,
        pool_code_hash,
        router_code_hash,
    })
}

#[allow(clippy::too_many_arguments)]
pub async fn read_v3_pool<M: Middleware>(
    provider: Arc<M>,
    pool: Address,
    router: Address,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<OnlinePoolRead, AdapterError> {
    let abi: Abi =
        serde_json::from_str(V3_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let c = Contract::new(pool, abi.clone(), provider.clone());
    let b = BlockId::Number(anchor_block.into());
    let t0 = Instant::now();
    let slot: (U256, i32, u16, u16, u16, u8, bool) = c
        .method::<_, (U256, i32, u16, u16, u16, u8, bool)>("slot0", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record("slot0", pool, t0.elapsed());
    let t0 = Instant::now();
    let liq: U256 = c
        .method::<_, U256>("liquidity", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record("liquidity", pool, t0.elapsed());
    let t0 = Instant::now();
    let fee: U256 = c
        .method::<_, U256>("fee", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record("fee", pool, t0.elapsed());
    // token0/token1 use the V3 ABI's own copy of those two view functions
    // (identical selectors/signatures to V2's) -- same cache, same key
    // shape, so a V2 and V3 pool never collide (different `pool` address).
    let (token0, token1) = cached_pool_tokens(
        &provider,
        pool,
        &abi,
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let pool_code_hash = cached_code_hash(
        &provider,
        pool,
        "get_code(pool)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let router_code_hash = cached_code_hash(
        &provider,
        router,
        "get_code(router)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    Ok(OnlinePoolRead {
        anchor_block,
        token0,
        token1,
        reserve0: None,
        reserve1: None,
        sqrt_price_x96: Some(slot.0),
        liquidity: Some(liq),
        fee: Some(fee.as_u32()),
        pool_code_hash,
        router_code_hash,
    })
}

const V2_FACTORY_READ_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenA","type":"address"},{"internalType":"address","name":"tokenB","type":"address"}],"name":"getPair","outputs":[{"internalType":"address","name":"pair","type":"address"}],"stateMutability":"view","type":"function"}]"#;
const V3_FACTORY_READ_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenA","type":"address"},{"internalType":"address","name":"tokenB","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"}],"name":"getPool","outputs":[{"internalType":"address","name":"pool","type":"address"}],"stateMutability":"view","type":"function"}]"#;

/// Resolves a V2 pair address via the venue's factory, pinned to
/// `anchor_block`. Returns `None` (not an error) when no pair exists for
/// this token combination at this venue.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_v2_pool_address<M: Middleware>(
    provider: Arc<M>,
    factory: Address,
    token_a: Address,
    token_b: Address,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<Option<Address>, AdapterError> {
    let key = V2PairKey::new(chain_id, factory, token_a, token_b);
    cache
        .v2_pair(key, anchor_block, || async move {
            let abi: Abi = serde_json::from_str(V2_FACTORY_READ_ABI)
                .map_err(|_| AdapterError::IncompleteMetadata)?;
            let c = Contract::new(factory, abi, provider);
            let t0 = Instant::now();
            let pair: Address = c
                .method::<_, Address>("getPair", (token_a, token_b))
                .map_err(|_| AdapterError::MissingState)?
                .block(BlockId::Number(anchor_block.into()))
                .call()
                .await
                .map_err(|_| AdapterError::MissingState)?;
            metrics.record("getPair", pair, t0.elapsed());
            if pair.is_zero() {
                Ok(Resolved::Negative {
                    observed_at_block: anchor_block,
                })
            } else {
                Ok(Resolved::Positive(pair))
            }
        })
        .await
}

/// Resolves a V3 pool address for a given fee tier via the venue's factory,
/// pinned to `anchor_block`. Returns `None` (not an error) when no pool
/// exists for this token/fee combination.
#[allow(clippy::too_many_arguments)]
pub async fn resolve_v3_pool_address<M: Middleware>(
    provider: Arc<M>,
    factory: Address,
    token_a: Address,
    token_b: Address,
    fee: u32,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<Option<Address>, AdapterError> {
    let key = V3PoolKey::new(chain_id, factory, token_a, token_b, fee);
    cache
        .v3_pool(key, anchor_block, || async move {
            let abi: Abi = serde_json::from_str(V3_FACTORY_READ_ABI)
                .map_err(|_| AdapterError::IncompleteMetadata)?;
            let c = Contract::new(factory, abi, provider);
            let t0 = Instant::now();
            let pool: Address = c
                .method::<_, Address>("getPool", (token_a, token_b, fee))
                .map_err(|_| AdapterError::MissingState)?
                .block(BlockId::Number(anchor_block.into()))
                .call()
                .await
                .map_err(|_| AdapterError::MissingState)?;
            metrics.record("getPool", pool, t0.elapsed());
            if pool.is_zero() {
                Ok(Resolved::Negative {
                    observed_at_block: anchor_block,
                })
            } else {
                Ok(Resolved::Positive(pool))
            }
        })
        .await
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
    metrics: &QuoteMetrics,
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
    let t0 = Instant::now();
    let amounts: Vec<U256> = c
        .method::<_, Vec<U256>>("getAmountsOut", (amount_in, vec![token_in, token_out]))
        .map_err(|_| AdapterError::MissingState)?
        .block(BlockId::Number(anchor_block.into()))
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record("getAmountsOut", pool, t0.elapsed());
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

#[allow(clippy::too_many_arguments)]
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
    metrics: &QuoteMetrics,
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
    let t0 = Instant::now();
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
    metrics.record("quoteExactInputSingle", pool, t0.elapsed());
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

#[cfg(test)]
mod metadata_cache_wiring_tests {
    //! Deterministic cache-off vs cache-on equivalence, against
    //! `ethers::providers::MockProvider` -- no network. Proves the same
    //! two things Phase 1 evidence gathering was blocked on: (a) resolved
    //! identity values are byte-identical whether served from cache or a
    //! fresh fetch, and (b) a shared cache measurably collapses the exact
    //! call counts this baseline round measured as duplicated.
    use super::*;
    use crate::core::canonical_metadata_cache::CanonicalMetadataCache;
    use ethers::abi::Token;
    use ethers::providers::{MockProvider, Provider};
    use ethers::types::Bytes;

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    fn encode_address(a: Address) -> Bytes {
        Bytes::from(ethers::abi::encode(&[Token::Address(a)]))
    }

    fn encode_reserves(r0: u64, r1: u64) -> Bytes {
        Bytes::from(ethers::abi::encode(&[
            Token::Uint(U256::from(r0)),
            Token::Uint(U256::from(r1)),
            Token::Uint(U256::zero()),
        ]))
    }

    fn fake_bytecode() -> Bytes {
        Bytes::from(vec![0x60, 0x80, 0x60, 0x40])
    }

    #[tokio::test]
    async fn v2_pair_resolution_direction_independent_and_cached() {
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let cache: CanonicalMetadataCache<Address, H256> = CanonicalMetadataCache::new();
        let metrics = QuoteMetrics::new();
        let factory = addr(10);
        let token_a = addr(1);
        let token_b = addr(2);
        let expected_pair = addr(999);

        // Exactly one response queued: A->B then B->A must canonicalize to
        // the same cache key and the second call must be a cache hit, not
        // a second eth_call. If the wiring were direction-dependent, the
        // second call would try to pop from an empty queue and error.
        mock.push::<Bytes, Bytes>(encode_address(expected_pair))
            .unwrap();

        let out_ab = resolve_v2_pool_address(
            provider.clone(),
            factory,
            token_a,
            token_b,
            100,
            137,
            &cache,
            &metrics,
        )
        .await
        .unwrap();
        let out_ba = resolve_v2_pool_address(
            provider.clone(),
            factory,
            token_b,
            token_a,
            100,
            137,
            &cache,
            &metrics,
        )
        .await
        .unwrap();

        assert_eq!(out_ab, Some(expected_pair));
        assert_eq!(
            out_ab, out_ba,
            "QUOTE_RESULT_EQUIVALENCE: A->B and B->A must resolve identically"
        );
        assert_eq!(cache.metrics().v2_pair.rpc_calls, 1);
    }

    #[tokio::test]
    async fn v2_pair_resolution_without_shared_cache_needs_one_response_per_call() {
        // Baseline shape (no cache reuse): two independent lookups against
        // two separate cache instances each need their own mocked
        // response. Contrasts directly with the test above -- same inputs,
        // same outputs, different real RPC volume.
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let metrics = QuoteMetrics::new();
        let factory = addr(10);
        let token_a = addr(1);
        let token_b = addr(2);
        let expected_pair = addr(999);

        mock.push::<Bytes, Bytes>(encode_address(expected_pair))
            .unwrap();
        mock.push::<Bytes, Bytes>(encode_address(expected_pair))
            .unwrap();

        let cache_1: CanonicalMetadataCache<Address, H256> = CanonicalMetadataCache::new();
        let out_1 = resolve_v2_pool_address(
            provider.clone(),
            factory,
            token_a,
            token_b,
            100,
            137,
            &cache_1,
            &metrics,
        )
        .await
        .unwrap();
        let cache_2: CanonicalMetadataCache<Address, H256> = CanonicalMetadataCache::new();
        let out_2 = resolve_v2_pool_address(
            provider.clone(),
            factory,
            token_b,
            token_a,
            100,
            137,
            &cache_2,
            &metrics,
        )
        .await
        .unwrap();

        assert_eq!(
            out_1, out_2,
            "QUOTE_RESULT_EQUIVALENCE holds regardless of cache sharing"
        );
    }

    #[tokio::test]
    async fn read_v2_pool_reuses_cached_metadata_across_calls() {
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let cache: CanonicalMetadataCache<Address, H256> = CanonicalMetadataCache::new();
        let metrics = QuoteMetrics::new();
        let pool = addr(30);
        let router = addr(40);
        let token0 = addr(1);
        let token1 = addr(2);

        // `MockProvider` is a LIFO stack (push_back / pop_back), so
        // responses are queued in REVERSE of consumption order. Real
        // consumption order is: getReserves, token0, token1,
        // get_code(pool), get_code(router) [first call], then getReserves
        // [second call] -- 6 responses total.
        mock.push::<Bytes, Bytes>(encode_reserves(150, 250))
            .unwrap();
        mock.push::<Bytes, Bytes>(fake_bytecode()).unwrap();
        mock.push::<Bytes, Bytes>(fake_bytecode()).unwrap();
        mock.push::<Bytes, Bytes>(encode_address(token1)).unwrap();
        mock.push::<Bytes, Bytes>(encode_address(token0)).unwrap();
        mock.push::<Bytes, Bytes>(encode_reserves(100, 200))
            .unwrap();
        // Second call, same pool/router: only getReserves is real
        // per-block state and must be fetched again. token0/token1/both
        // code hashes must come from cache -- only 1 response was queued
        // for it above; an uncached second call would try to pop 4 more
        // responses from an empty queue and fail loudly.

        let first = read_v2_pool(provider.clone(), pool, router, 100, 137, &cache, &metrics)
            .await
            .unwrap();
        let second = read_v2_pool(provider.clone(), pool, router, 100, 137, &cache, &metrics)
            .await
            .unwrap();

        assert_eq!(first.token0, second.token0);
        assert_eq!(first.token1, second.token1);
        assert_eq!(
            first.pool_code_hash, second.pool_code_hash,
            "QUOTE_RESULT_EQUIVALENCE: cached code hash must match the freshly-fetched one"
        );
        assert_eq!(first.router_code_hash, second.router_code_hash);
        // Reserves are real per-block state -- must NOT be equal (proves
        // the cache did not accidentally memoize state).
        assert_ne!(first.reserve0, second.reserve0);

        let m = cache.metrics();
        assert_eq!(m.token0.rpc_calls, 1);
        assert_eq!(m.token1.rpc_calls, 1);
        assert_eq!(
            m.code_hash.rpc_calls, 2,
            "one for pool, one for router -- each fetched once total"
        );
    }
}
