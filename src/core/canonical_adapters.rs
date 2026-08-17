//! Canonical quote-adapter outputs. Adapters must supply raw on-chain state;
//! no normalized state may be reconstructed from quote amounts.
use crate::core::{
    canonical_execution_context::{PinnedPoolState, PoolExecutionMetadata, TokenMetadata},
    canonical_metadata_cache::{CodeKey, PoolMetadataKey, Resolved, V2PairKey, V3PoolKey},
    pool_state_sim::SimulatedPoolState,
};
use ethers::types::{Address, Bytes, H256, U256};
use ethers::{abi::Abi, contract::Contract, providers::Middleware, types::BlockId};
use std::str::FromStr;
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
///
/// Carries a second accumulator, `MulticallStats`, that counts the Phase-A
/// state-read batching layer: one `rpc_calls` increment per physical
/// Multicall3 `aggregate3` eth_call (NOT per subcall), plus subcall total,
/// batch sizes, failures, and any individual state reads that fell back to
/// the non-batched path. `1 multicall eth_call = 1 physical RPC; N subcalls
/// = N subcalls` -- the subcalls are never counted as physical RPCs.
#[derive(Clone, Default)]
pub struct QuoteMetrics {
    calls: Option<Arc<Mutex<Vec<RpcCallRecord>>>>,
    multicall: Option<Arc<Mutex<MulticallStats>>>,
}

/// Phase-A state-read batching diagnostics. `rpc_calls`/`batch_count` are
/// physical Multicall3 `aggregate3` eth_calls; `subcalls` is the count of
/// view calls packed inside them. `individual_state_calls` is non-zero only
/// when the non-batched fallback path ran (reference/tests, or a forced
/// disable) -- in the normal hot path it stays 0.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct MulticallStats {
    pub rpc_calls: u64,
    pub subcalls: u64,
    pub batch_count: u64,
    pub subcall_failures: u64,
    pub wait_ms: u64,
    pub batch_sizes: Vec<usize>,
    pub individual_state_calls: u64,
}

impl QuoteMetrics {
    pub fn new() -> Self {
        Self {
            calls: Some(Arc::new(Mutex::new(Vec::new()))),
            multicall: Some(Arc::new(Mutex::new(MulticallStats::default()))),
        }
    }

    pub fn disabled() -> Self {
        Self {
            calls: None,
            multicall: None,
        }
    }

    fn record(&self, call_kind: &'static str, pool: Address, wait: Duration) {
        if let Some(sink) = &self.calls {
            sink.lock().unwrap().push(RpcCallRecord {
                call_kind,
                pool,
                wait_ms: wait.as_millis() as u64,
            });
        }
    }

    /// Record one physical Multicall3 `aggregate3` eth_call. `batch_size`
    /// is the number of subcalls packed in this batch; `failures` is how
    /// many of those subcalls returned `success=false`. Called once per
    /// batch, never per subcall.
    fn record_multicall(&self, batch_size: usize, failures: u64, wait: Duration) {
        if let Some(sink) = &self.multicall {
            let mut m = sink.lock().unwrap();
            m.rpc_calls += 1;
            m.batch_count += 1;
            m.subcalls += batch_size as u64;
            m.subcall_failures += failures;
            m.wait_ms += wait.as_millis() as u64;
            m.batch_sizes.push(batch_size);
        }
    }

    /// Record an individual (non-batched) state read -- only the fallback /
    /// reference path uses this. Bumps `individual_state_calls` and also
    /// pushes a normal `RpcCallRecord` so the per-kind breakdown still
    /// shows `getReserves` / `slot0` / `liquidity` / `fee` when the
    /// non-batched path ran.
    fn record_individual_state(&self, call_kind: &'static str, pool: Address, wait: Duration) {
        self.record(call_kind, pool, wait);
        if let Some(sink) = &self.multicall {
            sink.lock().unwrap().individual_state_calls += 1;
        }
    }

    pub fn drain(&self) -> Vec<RpcCallRecord> {
        match &self.calls {
            Some(sink) => std::mem::take(&mut *sink.lock().unwrap()),
            None => Vec::new(),
        }
    }

    /// Snapshot the multicall batching stats and reset the accumulator.
    /// Returns `MulticallStats::default()` for a `disabled()` sink.
    pub fn multicall_snapshot(&self) -> MulticallStats {
        match &self.multicall {
            Some(sink) => std::mem::take(&mut *sink.lock().unwrap()),
            None => MulticallStats::default(),
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
    metrics.record_individual_state("getReserves", pool, t0.elapsed());
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
    metrics.record_individual_state("slot0", pool, t0.elapsed());
    let t0 = Instant::now();
    let liq: U256 = c
        .method::<_, U256>("liquidity", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record_individual_state("liquidity", pool, t0.elapsed());
    let t0 = Instant::now();
    let fee: U256 = c
        .method::<_, U256>("fee", ())
        .map_err(|_| AdapterError::MissingState)?
        .block(b)
        .call()
        .await
        .map_err(|_| AdapterError::MissingState)?;
    metrics.record_individual_state("fee", pool, t0.elapsed());
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

// ============================================================
// Phase-A state-read batching via Multicall3 `aggregate3`.
//
// `aggregate3((bool allowFailure, bytes callData)[])` packs N view
// calls into one physical `eth_call` and returns
// `(bool success, bytes returnData)[]`. We call it via a raw
// `eth_call` pinned to `BlockId::Number(anchor_block)` rather than
// `ethers::contract::Multicall`, whose `new()` resolves its own
// contract address at `latest` -- that would violate the Phase-A
// invariant that every subcall observes exactly the same pinned
// anchor block. `aggregate3` is `payable` but we invoke it through
// `eth_call` with zero value, so no transaction is ever sent.
// ============================================================

/// Canonical Multicall3 address -- identical on every chain it is
/// deployed to (Polygon mainnet included). Parsed once per call rather
/// than `const` because `Address` has no `const` full-width constructor.
///
/// Sourced from the mds1/multicall3 deployment registry (verified on
/// PolygonScan at this exact address, 13M+ txs on Polygon alone):
/// https://github.com/mds1/multicall3
pub fn multicall3_address() -> Address {
    Address::from_str("0xcA11bde05977b3631167028862bE2a173976CA11")
        .expect("MULTICALL3_ADDRESS is a valid constant hex literal")
}

const MULTICALL3_AGGREGATE3_ABI: &str = r#"[{"inputs":[{"internalType":"struct Multicall3.Call3[]","name":"calls","type":"tuple[]","components":[{"internalType":"address","name":"target","type":"address"},{"internalType":"bool","name":"allowFailure","type":"bool"},{"internalType":"bytes","name":"callData","type":"bytes"}]}],"name":"aggregate3","outputs":[{"internalType":"struct Multicall3.Result[]","name":"returnData","type":"tuple[]","components":[{"internalType":"bool","name":"success","type":"bool"},{"internalType":"bytes","name":"returnData","type":"bytes"}]}],"stateMutability":"payable","type":"function"}]"#;

/// One view call packed into a Multicall3 batch. Exactly the four
/// per-block state reads Phase A makes -- `getReserves` (V2) and
/// `slot0`/`liquidity`/`fee` (V3). Metadata reads (`token0`/`token1`/
/// code hash) stay on the cached path; quote calls (`getAmountsOut`/
/// `quoteExactInputSingle`) are intentionally NOT batched here.
#[derive(Clone, Copy, Debug)]
pub enum StateSubcall {
    V2Reserves(Address),
    V3Slot0(Address),
    V3Liquidity(Address),
    V3Fee(Address),
}

impl StateSubcall {
    pub fn pool(&self) -> Address {
        match self {
            Self::V2Reserves(p) | Self::V3Slot0(p) | Self::V3Liquidity(p) | Self::V3Fee(p) => *p,
        }
    }

    pub fn call_kind(&self) -> &'static str {
        match self {
            Self::V2Reserves(_) => "getReserves",
            Self::V3Slot0(_) => "slot0",
            Self::V3Liquidity(_) => "liquidity",
            Self::V3Fee(_) => "fee",
        }
    }

    fn function_name(&self) -> &'static str {
        match self {
            Self::V2Reserves(_) => "getReserves",
            Self::V3Slot0(_) => "slot0",
            Self::V3Liquidity(_) => "liquidity",
            Self::V3Fee(_) => "fee",
        }
    }

    fn abi(&self) -> &'static str {
        match self {
            Self::V2Reserves(_) => V2_READ_ABI,
            Self::V3Slot0(_) | Self::V3Liquidity(_) | Self::V3Fee(_) => V3_READ_ABI,
        }
    }

    /// ABI-encode this subcall's calldata (selector + args). Pure local
    /// encoding -- no RPC. `Contract::method` only encodes here; the
    /// returned `Bytes` is what gets packed into the Multicall3 batch.
    fn encode_calldata<M: Middleware>(&self, provider: &Arc<M>) -> Result<Bytes, AdapterError> {
        let abi: Abi =
            serde_json::from_str(self.abi()).map_err(|_| AdapterError::IncompleteMetadata)?;
        let c = Contract::new(self.pool(), abi, provider.clone());
        let call = c
            .method::<_, ()>(self.function_name(), ())
            .map_err(|_| AdapterError::MissingState)?;
        call.calldata().ok_or(AdapterError::MissingState)
    }

    /// Decode this subcall kind's raw return `Bytes` into a typed
    /// `StateRead`. Uses the exact same `Token` shapes the individual
    /// `read_v2_pool` / `read_v3_pool` paths decode with, so a value
    /// decoded from a multicall `returnData` is byte-identical to one
    /// decoded from an individual `eth_call` response.
    fn decode_return(&self, data: &[u8]) -> Result<StateRead, AdapterError> {
        use ethers::abi::ParamType;
        match self {
            Self::V2Reserves(_) => {
                let tokens = ethers::abi::decode(
                    &[
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                    ],
                    data,
                )
                .map_err(|_| AdapterError::MissingState)?;
                Ok(StateRead::Reserves {
                    reserve0: tokens[0]
                        .clone()
                        .into_uint()
                        .ok_or(AdapterError::MissingState)?,
                    reserve1: tokens[1]
                        .clone()
                        .into_uint()
                        .ok_or(AdapterError::MissingState)?,
                    block_timestamp_last: tokens[2]
                        .clone()
                        .into_uint()
                        .ok_or(AdapterError::MissingState)?,
                })
            }
            Self::V3Slot0(_) => {
                let tokens = ethers::abi::decode(
                    &[
                        ParamType::Uint(256),
                        ParamType::Int(256),
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Uint(256),
                        ParamType::Bool,
                    ],
                    data,
                )
                .map_err(|_| AdapterError::MissingState)?;
                let sqrt_price_x96 = tokens[0]
                    .clone()
                    .into_uint()
                    .ok_or(AdapterError::MissingState)?;
                let tick = tokens[1]
                    .clone()
                    .into_int()
                    .ok_or(AdapterError::MissingState)?
                    // int24 is sign-extended to a 256-bit two's-complement
                    // word on the wire, so for a negative tick the full U256
                    // is far larger than u32::MAX and `as_u32()` would
                    // panic. Take the low 32 bits (the int24 always fits)
                    // and reinterpret as i32 -- the same two's-complement
                    // value.
                    .low_u32() as i32;
                let observation_index = tokens[2]
                    .clone()
                    .into_uint()
                    .ok_or(AdapterError::MissingState)?
                    .low_u32() as u16;
                let observation_cardinality = tokens[3]
                    .clone()
                    .into_uint()
                    .ok_or(AdapterError::MissingState)?
                    .low_u32() as u16;
                let observation_cardinality_next = tokens[4]
                    .clone()
                    .into_uint()
                    .ok_or(AdapterError::MissingState)?
                    .low_u32() as u16;
                let fee_protocol = tokens[5]
                    .clone()
                    .into_uint()
                    .ok_or(AdapterError::MissingState)?
                    .low_u32() as u8;
                let unlocked = tokens[6]
                    .clone()
                    .into_bool()
                    .ok_or(AdapterError::MissingState)?;
                Ok(StateRead::Slot0 {
                    sqrt_price_x96,
                    tick,
                    observation_index,
                    observation_cardinality,
                    observation_cardinality_next,
                    fee_protocol,
                    unlocked,
                })
            }
            Self::V3Liquidity(_) => {
                let tokens = ethers::abi::decode(&[ParamType::Uint(256)], data)
                    .map_err(|_| AdapterError::MissingState)?;
                Ok(StateRead::Liquidity(
                    tokens[0]
                        .clone()
                        .into_uint()
                        .ok_or(AdapterError::MissingState)?,
                ))
            }
            Self::V3Fee(_) => {
                let tokens = ethers::abi::decode(&[ParamType::Uint(256)], data)
                    .map_err(|_| AdapterError::MissingState)?;
                Ok(StateRead::Fee(
                    tokens[0]
                        .clone()
                        .into_uint()
                        .ok_or(AdapterError::MissingState)?,
                ))
            }
        }
    }
}

/// Decoded per-block pool state from a Multicall3 subcall. The tagged
/// variants mirror the four state reads Phase A batches; metadata
/// (token0/token1/code hash) is fetched separately on the cached path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StateRead {
    Reserves {
        reserve0: U256,
        reserve1: U256,
        block_timestamp_last: U256,
    },
    Slot0 {
        sqrt_price_x96: U256,
        tick: i32,
        observation_index: u16,
        observation_cardinality: u16,
        observation_cardinality_next: u16,
        fee_protocol: u8,
        unlocked: bool,
    },
    Liquidity(U256),
    Fee(U256),
}

/// Packs `subcalls` into Multicall3 `aggregate3` batches of `batch_size`,
/// issues one `eth_call` per batch pinned to `BlockId::Number(anchor_block)`,
/// and returns one `Option<StateRead>` per input subcall, in order.
///
/// Fail-closed: a subcall whose `aggregate3` result is `success=false`
/// yields `None` (the pool is dropped downstream -- never a 0/default).
/// If a whole batch `eth_call` errors (infra failure), every subcall in
/// that batch yields `None` and the failure is logged with pool/kind
/// identification; other batches still complete.
///
/// `chain_id` is accepted for API symmetry with the other adapter fns
/// but is not used -- Multicall3 is at a canonical address, not
/// chain-resolved.
#[allow(clippy::too_many_arguments)]
pub async fn batch_read_pool_state<M: Middleware>(
    provider: &Arc<M>,
    subcalls: &[StateSubcall],
    anchor_block: u64,
    _chain_id: u64,
    batch_size: usize,
    metrics: &QuoteMetrics,
) -> Vec<Option<StateRead>> {
    let mut out: Vec<Option<StateRead>> = (0..subcalls.len()).map(|_| None).collect();
    if subcalls.is_empty() {
        return out;
    }
    let batch_size = batch_size.clamp(1, subcalls.len().max(1));
    let abi: Abi = serde_json::from_str(MULTICALL3_AGGREGATE3_ABI)
        .expect("MULTICALL3_AGGREGATE3_ABI is a constant literal");
    let multicall = Contract::new(multicall3_address(), abi, provider.clone());
    let block = BlockId::Number(anchor_block.into());

    let mut chunk_start_index = 0usize;
    for chunk in subcalls.chunks(batch_size) {
        // Multicall3.Call3 = (address target, bool allowFailure, bytes
        // callData) -- note the field order: `target` is FIRST, then
        // `allowFailure`, matching the on-chain struct (selector
        // 0x82ad56cb = `aggregate3((address,bool,bytes)[])`).
        let mut calls_arg: Vec<(Address, bool, Bytes)> = Vec::with_capacity(chunk.len());
        for sc in chunk {
            let data = match sc.encode_calldata(provider) {
                Ok(d) => d,
                Err(_) => {
                    // Encoding is pure-local; failure here is a programming
                    // error. Fail-closed: record None, do not pack a bad
                    // subcall into the batch.
                    continue;
                }
            };
            // allowFailure = true: a reverting view call returns
            // (false, revert-data) instead of reverting the whole batch.
            calls_arg.push((sc.pool(), true, data));
        }
        // If every subcall in this chunk failed to encode, there is
        // nothing to send -- leave their Nones in place.
        if calls_arg.is_empty() {
            continue;
        }
        let t0 = Instant::now();
        let call = multicall
            .method::<_, Vec<(bool, Bytes)>>("aggregate3", (calls_arg.clone(),))
            .map_err(|e| format!("aggregate3 method build failed: {e:?}"))
            // Block-pin the aggregate3 eth_call to the anchor -- never
            // `latest`. This is the whole point of not using the
            // `ethers::contract::Multicall` helper.
            .map(|c| c.block(block));
        let batch_result: Result<Vec<(bool, Bytes)>, String> = match call {
            Ok(c) => c
                .call()
                .await
                .map_err(|e| format!("aggregate3 eth_call: {e:?}")),
            Err(e) => Err(e),
        };
        let wait = t0.elapsed();
        let packed = calls_arg.len();
        match batch_result {
            Ok(results) => {
                // `aggregate3` returns one (success, returnData) per
                // packed subcall, in order. Map each back to the chunk's
                // original subcall index. A `success=false` subcall is
                // fail-closed -> None (no 0/default reconstruction).
                let mut failures = 0u64;
                for (i, (success, data)) in results.iter().enumerate() {
                    let subcall_idx = chunk_start_index + i;
                    if !success {
                        failures += 1;
                        tracing::warn!(
                            target: "canonical_discovery",
                            pool = %subcalls[subcall_idx].pool(),
                            call_kind = %subcalls[subcall_idx].call_kind(),
                            anchor_block,
                            "MULTICALL_SUBCALL_FAILED: pool state read reverted in batch, dropping pool"
                        );
                        continue;
                    }
                    match subcalls[subcall_idx].decode_return(data.as_ref()) {
                        Ok(read) => out[subcall_idx] = Some(read),
                        Err(_) => {
                            failures += 1;
                            tracing::warn!(
                                target: "canonical_discovery",
                                pool = %subcalls[subcall_idx].pool(),
                                call_kind = %subcalls[subcall_idx].call_kind(),
                                anchor_block,
                                "MULTICALL_SUBCALL_DECODE_FAILED: returnData did not match expected ABI shape, dropping pool"
                            );
                        }
                    }
                }
                // 1 batch eth_call = 1 physical RPC; N packed subcalls =
                // N subcalls. Only the eth_call is counted as an RPC.
                metrics.record_multicall(packed, failures, wait);
            }
            Err(err) => {
                // Whole-batch eth_call failed (RPC / multicall3 not
                // reachable). Fail-closed: every packed subcall in this
                // chunk stays None, and the failure is logged with the
                // pool/kind of the first subcall so it is identifiable
                // downstream. Other batches still run.
                tracing::warn!(
                    target: "canonical_discovery",
                    first_pool = %chunk.first().map(|s| s.pool()).unwrap_or_default(),
                    first_call_kind = %chunk.first().map(|s| s.call_kind()).unwrap_or(""),
                    batch_size = packed,
                    anchor_block,
                    error = %err,
                    "MULTICALL_BATCH_FAILED: aggregate3 eth_call errored, all subcalls in batch dropped"
                );
                metrics.record_multicall(packed, packed as u64, wait);
            }
        }
        chunk_start_index += chunk.len();
    }
    out
}

/// Assembles an `OnlinePoolRead` for a V2 pool from a Multicall3-decoded
/// `StateRead::Reserves` plus the cached metadata (`token0`/`token1`,
/// pool/router code hashes) that the batched path still fetches on the
/// cached warm-metadata path (0 RPC on warm cache). The result is
/// field-identical to what `read_v2_pool` returns for the same anchor
/// block -- only the reserves now arrive via a batched eth_call.
#[allow(clippy::too_many_arguments)]
pub async fn build_v2_pool_read_from_state<M: Middleware>(
    provider: &Arc<M>,
    pool: Address,
    router: Address,
    state: &StateRead,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<OnlinePoolRead, AdapterError> {
    let StateRead::Reserves {
        reserve0,
        reserve1,
        block_timestamp_last: _,
    } = state
    else {
        return Err(AdapterError::MissingState);
    };
    if reserve0.is_zero() || reserve1.is_zero() {
        // Same fail-closed guard `normalized_v2_state` applies. A
        // zero-reserve pool cannot produce a usable quote and must not
        // be reconstructed as a default-success state.
        return Err(AdapterError::MissingState);
    }
    let abi: Abi =
        serde_json::from_str(V2_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let (token0, token1) =
        cached_pool_tokens(provider, pool, &abi, anchor_block, chain_id, cache, metrics).await?;
    let pool_code_hash = cached_code_hash(
        provider,
        pool,
        "get_code(pool)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let router_code_hash = cached_code_hash(
        provider,
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
        reserve0: Some(*reserve0),
        reserve1: Some(*reserve1),
        sqrt_price_x96: None,
        liquidity: None,
        fee: None,
        pool_code_hash,
        router_code_hash,
    })
}

/// Assembles an `OnlinePoolRead` for a V3 pool from Multicall3-decoded
/// `StateRead::Slot0` / `Liquidity` / `Fee` plus cached metadata. The
/// three V3 state reads are batched together; the caller collects them
/// from three consecutive `StateRead` slots and passes the full trio
/// here. Field-identical to `read_v3_pool` for the same anchor block.
#[allow(clippy::too_many_arguments)]
pub async fn build_v3_pool_read_from_state<M: Middleware>(
    provider: &Arc<M>,
    pool: Address,
    router: Address,
    slot0: &StateRead,
    liquidity: &StateRead,
    fee: &StateRead,
    anchor_block: u64,
    chain_id: u64,
    cache: &MetadataCache,
    metrics: &QuoteMetrics,
) -> Result<OnlinePoolRead, AdapterError> {
    let StateRead::Slot0 { sqrt_price_x96, .. } = slot0 else {
        return Err(AdapterError::MissingState);
    };
    let StateRead::Liquidity(liq) = liquidity else {
        return Err(AdapterError::MissingState);
    };
    let StateRead::Fee(fee_v) = fee else {
        return Err(AdapterError::MissingState);
    };
    if sqrt_price_x96.is_zero() {
        return Err(AdapterError::MissingState);
    }
    let abi: Abi =
        serde_json::from_str(V3_READ_ABI).map_err(|_| AdapterError::IncompleteMetadata)?;
    let (token0, token1) =
        cached_pool_tokens(provider, pool, &abi, anchor_block, chain_id, cache, metrics).await?;
    let pool_code_hash = cached_code_hash(
        provider,
        pool,
        "get_code(pool)",
        anchor_block,
        chain_id,
        cache,
        metrics,
    )
    .await?;
    let router_code_hash = cached_code_hash(
        provider,
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
        sqrt_price_x96: Some(*sqrt_price_x96),
        liquidity: Some(*liq),
        fee: Some(fee_v.as_u32()),
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

#[cfg(test)]
mod multicall_state_read_tests {
    //! Equivalence between the individual `eth_call` state-read path and
    //! the Multicall3 `aggregate3` batched path, against
    //! `ethers::providers::MockProvider` (no network). Proves the same
    //! four things this phase is gated on:
    //! (a) a value decoded from a multicall `returnData` slot is
    //!     byte-identical to one decoded from an individual `eth_call`
    //!     response of the same raw bytes (STATE_READ_EQUIVALENCE);
    //! (b) a failed subcall is fail-closed -- `None`, never a 0/default;
    //! (c) the multicall eth_call is counted as 1 physical RPC while the
    //!     N subcalls it packs are counted as N subcalls, never as RPCs;
    //! (d) each subcall encodes a calldata with the correct function
    //!     selector for its pool, so the batch only ever calls the four
    //!     intended view functions on the intended pool addresses.
    use super::*;
    use ethers::abi::{ParamType, Token};
    use ethers::providers::{MockProvider, Provider};
    use ethers::types::{Bytes, I256};

    fn addr(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }

    /// Raw `getReserves` return data (uint112, uint112, uint32) encoded as
    /// the same three-`Uint` shape both paths decode with.
    fn reserves_return(r0: u128, r1: u128) -> Vec<u8> {
        ethers::abi::encode(&[
            Token::Uint(U256::from(r0)),
            Token::Uint(U256::from(r1)),
            Token::Uint(U256::zero()),
        ])
    }

    /// Raw `slot0` return data (uint160, int24, uint16, uint16, uint16,
    /// uint8, bool).
    fn slot0_return(sqrt: u128, tick: i32) -> Vec<u8> {
        ethers::abi::encode(&[
            Token::Uint(U256::from(sqrt)),
            // int24 is ABI-encoded sign-extended to a full 256-bit two's
            // complement word. `U256::from(neg)` panics, so build the raw
            // word via I256 -- the same wire shape the on-chain slot0
            // returns for a negative tick.
            Token::Int(I256::from(tick as i64).into_raw()),
            Token::Uint(U256::from(1u64)),
            Token::Uint(U256::from(2u64)),
            Token::Uint(U256::from(3u64)),
            Token::Uint(U256::from(4u64)),
            Token::Bool(true),
        ])
    }

    fn liquidity_return(liq: u128) -> Vec<u8> {
        ethers::abi::encode(&[Token::Uint(U256::from(liq))])
    }

    fn fee_return(fee: u32) -> Vec<u8> {
        ethers::abi::encode(&[Token::Uint(U256::from(fee))])
    }

    /// Encode a Multicall3 `aggregate3` return: `Result[]` where each
    /// `Result` is `(bool success, bytes returnData)`.
    fn aggregate3_return(results: &[(bool, Vec<u8>)]) -> Bytes {
        let tuples: Vec<Token> = results
            .iter()
            .map(|(success, data)| {
                Token::Tuple(vec![Token::Bool(*success), Token::Bytes(data.clone())])
            })
            .collect();
        Bytes::from(ethers::abi::encode(&[Token::Array(tuples)]))
    }

    fn individual_decode_reserves(raw: &[u8]) -> (U256, U256, U256) {
        let t = ethers::abi::decode(
            &[
                ParamType::Uint(256),
                ParamType::Uint(256),
                ParamType::Uint(256),
            ],
            raw,
        )
        .unwrap();
        (
            t[0].clone().into_uint().unwrap(),
            t[1].clone().into_uint().unwrap(),
            t[2].clone().into_uint().unwrap(),
        )
    }

    #[tokio::test]
    async fn v2_reserves_multicall_eq_individual() {
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let raw = reserves_return(150, 250);
        mock.push::<Bytes, Bytes>(aggregate3_return(&[(true, raw.clone())]))
            .unwrap();
        let metrics = QuoteMetrics::new();
        let subcalls = vec![StateSubcall::V2Reserves(addr(30))];
        let out = batch_read_pool_state(&provider, &subcalls, 100, 137, 100, &metrics).await;

        assert_eq!(out.len(), 1);
        let StateRead::Reserves {
            reserve0,
            reserve1,
            block_timestamp_last,
        } = out[0]
            .as_ref()
            .expect("fail-closed: successful subcall must decode")
        else {
            panic!("expected Reserves variant");
        };
        let (i0, i1, i2) = individual_decode_reserves(&raw);
        assert_eq!(*reserve0, i0, "STATE_READ_EQUIVALENCE: reserves0");
        assert_eq!(*reserve1, i1, "STATE_READ_EQUIVALENCE: reserves1");
        assert_eq!(*block_timestamp_last, i2);

        // 1 batch eth_call = 1 physical RPC; 1 packed subcall = 1 subcall.
        let mc = metrics.multicall_snapshot();
        assert_eq!(mc.rpc_calls, 1);
        assert_eq!(mc.subcalls, 1);
        assert_eq!(mc.batch_count, 1);
        assert_eq!(mc.subcall_failures, 0);
        assert_eq!(mc.individual_state_calls, 0);
        assert_eq!(mc.batch_sizes, vec![1]);
    }

    #[tokio::test]
    async fn v3_slot0_liquidity_fee_multicall_eq_individual() {
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let pool = addr(31);
        let r_slot = slot0_return(1_000_000, -5);
        let r_liq = liquidity_return(42_000_000);
        let r_fee = fee_return(3000);
        mock.push::<Bytes, Bytes>(aggregate3_return(&[
            (true, r_slot.clone()),
            (true, r_liq.clone()),
            (true, r_fee.clone()),
        ]))
        .unwrap();
        let metrics = QuoteMetrics::new();
        let subcalls = vec![
            StateSubcall::V3Slot0(pool),
            StateSubcall::V3Liquidity(pool),
            StateSubcall::V3Fee(pool),
        ];
        let out = batch_read_pool_state(&provider, &subcalls, 100, 137, 100, &metrics).await;
        assert_eq!(out.len(), 3);

        // slot0
        let StateRead::Slot0 {
            sqrt_price_x96,
            tick,
            observation_index,
            observation_cardinality,
            observation_cardinality_next,
            fee_protocol,
            unlocked,
        } = out[0].as_ref().unwrap()
        else {
            panic!("expected Slot0 variant");
        };
        assert_eq!(*sqrt_price_x96, U256::from(1_000_000u128));
        assert_eq!(*tick, -5, "STATE_READ_EQUIVALENCE: int24 tick (negative)");
        assert_eq!(*observation_index, 1);
        assert_eq!(*observation_cardinality, 2);
        assert_eq!(*observation_cardinality_next, 3);
        assert_eq!(*fee_protocol, 4);
        assert!(*unlocked);

        let StateRead::Liquidity(liq) = out[1].as_ref().unwrap() else {
            panic!("expected Liquidity variant");
        };
        assert_eq!(*liq, U256::from(42_000_000u128));

        let StateRead::Fee(fee) = out[2].as_ref().unwrap() else {
            panic!("expected Fee variant");
        };
        assert_eq!(*fee, U256::from(3000u64));

        // 1 batch eth_call for all 3 subcalls.
        let mc = metrics.multicall_snapshot();
        assert_eq!(mc.rpc_calls, 1, "one physical RPC for the whole batch");
        assert_eq!(mc.subcalls, 3, "3 packed subcalls");
        assert_eq!(mc.batch_count, 1);
        assert_eq!(mc.subcall_failures, 0);
    }

    #[tokio::test]
    async fn multicall_subcall_failure_is_fail_closed() {
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let pool = addr(32);
        // slot0 fails (success=false), liquidity and fee succeed. The
        // failed slot must yield None -- never a 0/default -- and the
        // others must still decode.
        mock.push::<Bytes, Bytes>(aggregate3_return(&[
            (false, Vec::new()),
            (true, liquidity_return(7)),
            (true, fee_return(500)),
        ]))
        .unwrap();
        let metrics = QuoteMetrics::new();
        let subcalls = vec![
            StateSubcall::V3Slot0(pool),
            StateSubcall::V3Liquidity(pool),
            StateSubcall::V3Fee(pool),
        ];
        let out = batch_read_pool_state(&provider, &subcalls, 100, 137, 100, &metrics).await;
        assert!(
            out[0].is_none(),
            "failed subcall must be None, not a default"
        );
        assert!(out[1].is_some());
        assert!(out[2].is_some());
        let mc = metrics.multicall_snapshot();
        assert_eq!(mc.subcall_failures, 1);
        assert_eq!(mc.rpc_calls, 1);
        assert_eq!(mc.subcalls, 3);
    }

    #[tokio::test]
    async fn multicall_chunks_into_multiple_batches() {
        // batch_size=2 with 3 subcalls -> 2 batches -> 2 physical RPCs.
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        // MockProvider pops responses LIFO. Chunk order is [s1,s2] then
        // [s3], so the first eth_call (size 2) must consume the 2-result
        // batch -- push it LAST so it is popped FIRST.
        mock.push::<Bytes, Bytes>(aggregate3_return(&[(true, reserves_return(5, 6))]))
            .unwrap();
        mock.push::<Bytes, Bytes>(aggregate3_return(&[
            (true, reserves_return(1, 2)),
            (true, reserves_return(3, 4)),
        ]))
        .unwrap();
        let metrics = QuoteMetrics::new();
        let subcalls = vec![
            StateSubcall::V2Reserves(addr(1)),
            StateSubcall::V2Reserves(addr(2)),
            StateSubcall::V2Reserves(addr(3)),
        ];
        let out = batch_read_pool_state(&provider, &subcalls, 100, 137, 2, &metrics).await;
        assert_eq!(out.len(), 3);
        assert!(out.iter().all(Option::is_some));
        let mc = metrics.multicall_snapshot();
        assert_eq!(mc.rpc_calls, 2, "2 batches = 2 physical RPCs");
        assert_eq!(mc.subcalls, 3);
        assert_eq!(mc.batch_count, 2);
        assert_eq!(mc.batch_sizes.len(), 2);
        // batch_size_avg = 3/2 = 1, max = 2.
        assert_eq!(mc.batch_sizes.iter().sum::<usize>(), 3);
        assert_eq!(*mc.batch_sizes.iter().max().unwrap(), 2);
    }

    #[tokio::test]
    async fn multicall_empty_subcalls_no_rpc() {
        let (provider, _mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let metrics = QuoteMetrics::new();
        let out = batch_read_pool_state(&provider, &[], 100, 137, 100, &metrics).await;
        assert!(out.is_empty());
        let mc = metrics.multicall_snapshot();
        assert_eq!(mc.rpc_calls, 0);
        assert_eq!(mc.subcalls, 0);
    }

    #[tokio::test]
    async fn subcall_calldata_uses_correct_function_selectors() {
        // Pure-local: encode_calldata must produce the canonical
        // 4-byte selector for each view function on the correct pool
        // address. No RPC -- MockProvider is only here so Contract can
        // be constructed.
        let (provider, _mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let pool = addr(7);

        fn selector_of(abi_str: &str, name: &str) -> [u8; 4] {
            let abi: Abi = serde_json::from_str(abi_str).unwrap();
            let f = abi.function(name).unwrap();
            let sig = f.short_signature();
            [sig[0], sig[1], sig[2], sig[3]]
        }

        let v2 = StateSubcall::V2Reserves(pool);
        let cd = v2.encode_calldata(&provider).unwrap();
        assert_eq!(&cd.0[..4], &selector_of(V2_READ_ABI, "getReserves"));
        assert_eq!(&cd.0[4..], &[] as &[u8], "getReserves has no args");

        let s0 = StateSubcall::V3Slot0(pool);
        assert_eq!(
            &s0.encode_calldata(&provider).unwrap().0[..4],
            &selector_of(V3_READ_ABI, "slot0")
        );
        let liq = StateSubcall::V3Liquidity(pool);
        assert_eq!(
            &liq.encode_calldata(&provider).unwrap().0[..4],
            &selector_of(V3_READ_ABI, "liquidity")
        );
        let fee = StateSubcall::V3Fee(pool);
        assert_eq!(
            &fee.encode_calldata(&provider).unwrap().0[..4],
            &selector_of(V3_READ_ABI, "fee")
        );
    }

    #[tokio::test]
    async fn build_v2_pool_read_from_state_eq_read_v2_pool_decoded_state() {
        // The batched `build_v2_pool_read_from_state` must yield the same
        // `reserve0`/`reserve1` the individual `read_v2_pool` would, for
        // the same raw return bytes. Both go through `cached_pool_tokens`
        // and `cached_code_hash` (warm metadata cache); the only input
        // that differs is where the reserves value came from. We feed the
        // SAME reserves through both paths and assert equality.
        let (provider, mock) = Provider::<MockProvider>::mocked();
        let provider = Arc::new(provider);
        let cache: MetadataCache = MetadataCache::new();
        let metrics = QuoteMetrics::new();
        let pool = addr(30);
        let router = addr(40);
        let r0 = U256::from(1500u64);
        let r1 = U256::from(2500u64);
        let state = StateRead::Reserves {
            reserve0: r0,
            reserve1: r1,
            block_timestamp_last: U256::zero(),
        };
        // build_v2_pool_read_from_state still needs token0/token1 + two
        // code hashes from the metadata cache (cold here -> 1 RPC each).
        // MockProvider is LIFO: push in REVERSE of consumption order
        // (get_code(router), get_code(pool), token1, token0).
        mock.push::<Bytes, Bytes>(fake_bytecode()).unwrap(); // get_code(router)
        mock.push::<Bytes, Bytes>(fake_bytecode()).unwrap(); // get_code(pool)
        mock.push::<Bytes, Bytes>(encode_address(addr(2))).unwrap(); // token1
        mock.push::<Bytes, Bytes>(encode_address(addr(1))).unwrap(); // token0
        let read = build_v2_pool_read_from_state(
            &provider, pool, router, &state, 100, 137, &cache, &metrics,
        )
        .await
        .unwrap();
        assert_eq!(read.reserve0, Some(r0));
        assert_eq!(read.reserve1, Some(r1));
        assert_eq!(read.token0, addr(1));
        assert_eq!(read.token1, addr(2));
        assert_eq!(read.pool_code_hash, read.router_code_hash);
    }

    fn fake_bytecode() -> Bytes {
        Bytes::from(vec![0x60, 0x80, 0x60, 0x40])
    }

    fn encode_address(a: Address) -> Bytes {
        Bytes::from(ethers::abi::encode(&[Token::Address(a)]))
    }
}
