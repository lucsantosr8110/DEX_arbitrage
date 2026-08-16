// ============================================================
// src/dex/pool_state_cache.rs — TTL cache para pool state
// ============================================================
//
// Reduz RPC calls em rounds back-to-back. TTL 12s = 1 Polygon block;
// pair `(reserve_a, reserve_b)` por pool é cacheado para hit em rounds
// vizinhos sem refazer multicall.
//
// Reusa padrão de `liquidity.rs:42-128` (POOL_ADDR_CACHE). Não usar
// crates externos (lru/moka) — Mutex<HashMap> basta, dataset < 200 pools.

use ethers::types::{Address, U256};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::trace;

/// Estado cacheado de um pool V2 (UniswapV2 / SushiSwap / QuickSwap).
#[derive(Debug, Clone, Copy)]
pub struct V2PoolState {
    pub reserve_a: U256,
    pub reserve_b: U256,
    pub fetched_at: Instant,
}

/// Estado cacheado de um pool V3.
#[derive(Debug, Clone, Copy)]
pub struct V3PoolState {
    pub sqrt_price_x96: U256,
    pub liquidity: u128,
    pub tick: i32,
    pub fetched_at: Instant,
}

#[derive(Default)]
struct CacheInner {
    v2: HashMap<Address, V2PoolState>,
    v3: HashMap<(Address, u32), V3PoolState>, // (pool, fee_tier)
}

/// Cache global estático (1 instância para todo o processo).
/// RwLock permite leituras concorrentes; writes são raros (~1/round).
static POOL_STATE_CACHE: once_cell::sync::Lazy<Arc<RwLock<CacheInner>>> =
    once_cell::sync::Lazy::new(|| Arc::new(RwLock::new(CacheInner::default())));

const DEFAULT_TTL: Duration = Duration::from_secs(12);

/// Hit ou miss para V2 pool.
pub async fn get_v2(pool: Address) -> Option<V2PoolState> {
    let cache = POOL_STATE_CACHE.read().await;
    cache.v2.get(&pool).and_then(|s| {
        if s.fetched_at.elapsed() < DEFAULT_TTL {
            Some(*s)
        } else {
            None
        }
    })
}

pub async fn put_v2(pool: Address, state: V2PoolState) {
    let mut cache = POOL_STATE_CACHE.write().await;
    cache.v2.insert(pool, state);
    trace!(target: "pool_state_cache", "v2 put {:?}", pool);
}

pub async fn invalidate_v2(pool: Address) {
    let mut cache = POOL_STATE_CACHE.write().await;
    cache.v2.remove(&pool);
}

/// Hit ou miss para V3 pool com fee tier específico.
pub async fn get_v3(pool: Address, fee_tier: u32) -> Option<V3PoolState> {
    let cache = POOL_STATE_CACHE.read().await;
    cache.v3.get(&(pool, fee_tier)).and_then(|s| {
        if s.fetched_at.elapsed() < DEFAULT_TTL {
            Some(*s)
        } else {
            None
        }
    })
}

pub async fn put_v3(pool: Address, fee_tier: u32, state: V3PoolState) {
    let mut cache = POOL_STATE_CACHE.write().await;
    cache.v3.insert((pool, fee_tier), state);
    trace!(target: "pool_state_cache", "v3 put {:?}/{}", pool, fee_tier);
}

/// Estatísticas para telemetria/debug.
pub async fn stats() -> (usize, usize) {
    let cache = POOL_STATE_CACHE.read().await;
    (cache.v2.len(), cache.v3.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn v2_round_trip() {
        let pool = Address::from_low_u64_be(0x1234);
        let state = V2PoolState {
            reserve_a: U256::from(1000u64),
            reserve_b: U256::from(2000u64),
            fetched_at: Instant::now(),
        };
        assert!(get_v2(pool).await.is_none());
        put_v2(pool, state).await;
        let got = get_v2(pool).await.unwrap();
        assert_eq!(got.reserve_a, state.reserve_a);
        assert_eq!(got.reserve_b, state.reserve_b);
    }

    #[tokio::test]
    async fn v3_round_trip() {
        let pool = Address::from_low_u64_be(0x5678);
        put_v3(pool, 3000, V3PoolState {
            sqrt_price_x96: U256::from(42u64),
            liquidity: 100,
            tick: -100,
            fetched_at: Instant::now(),
        }).await;
        let got = get_v3(pool, 3000).await.unwrap();
        assert_eq!(got.tick, -100);
        assert!(get_v3(pool, 500).await.is_none()); // tier diferente = miss
    }

    #[tokio::test]
    async fn v2_invalidate() {
        let pool = Address::from_low_u64_be(0x9abc);
        put_v2(pool, V2PoolState {
            reserve_a: U256::zero(),
            reserve_b: U256::zero(),
            fetched_at: Instant::now(),
        }).await;
        assert!(get_v2(pool).await.is_some());
        invalidate_v2(pool).await;
        assert!(get_v2(pool).await.is_none());
    }
}
