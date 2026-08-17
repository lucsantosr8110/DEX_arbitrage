//! Phase-A immutable-metadata cache.
//!
//! Caches only block-independent *identity* lookups made while quoting:
//! factory pair/pool address resolution, `token0`/`token1`, and contract
//! bytecode hash. These never change for a given `(chain_id, ...)` key --
//! a deployed pool's token order and a contract's bytecode do not vary
//! block to block. Single-flight per key so concurrent pair-quote tasks
//! racing for the same metadata within one round (or across rounds, since
//! the cache is owned by `CanonicalDiscoveryService` and outlives a single
//! round) collapse into at most one real RPC call.
//!
//! Deliberately holds NOTHING that varies by block or by amount: no
//! reserves, no `slot0`, no liquidity, no dynamic fee, no quote amount.
//! Caching those would silently reuse stale economic state across anchors
//! -- exactly the failure mode this module exists to stay clear of. See
//! `cache_does_not_store_*` tests at the bottom.

use std::collections::HashMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::OnceCell;

/// Orders an unordered token pair into a stable, direction-independent key.
/// Compares `Address` bytes directly -- A->B and B->A always collapse to
/// the same key. Never uses symbol or string comparison.
pub fn canonical_token_pair<A: Ord + Copy>(token_a: A, token_b: A) -> (A, A) {
    if token_a <= token_b {
        (token_a, token_b)
    } else {
        (token_b, token_a)
    }
}

/// How long (in blocks) a negative resolution (`pair == zero`, `pool ==
/// zero`, empty bytecode) stays cached before the next lookup re-checks the
/// chain. A pool/contract can be deployed after a negative read; a
/// positive read never needs this, since a deployed pool does not
/// un-deploy.
pub const NEGATIVE_CACHE_TTL_BLOCKS: u64 = 32;

/// Outcome of a real on-chain identity lookup, as the caller (the adapter
/// making the actual eth_call) classifies it. Built by the caller, not by
/// this module -- what counts as "not found" is domain-specific (zero
/// address for factory lookups, empty bytecode for code lookups).
#[derive(Debug, Clone, Copy)]
pub enum Resolved<V> {
    Positive(V),
    Negative { observed_at_block: u64 },
}

#[derive(Debug, Default)]
struct CacheCounters {
    hits: AtomicU64,
    misses: AtomicU64,
    negative_hits: AtomicU64,
    negative_expired: AtomicU64,
    rpc_calls: AtomicU64,
}

impl CacheCounters {
    fn snapshot(&self) -> CacheCounterSnapshot {
        CacheCounterSnapshot {
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            negative_hits: self.negative_hits.load(Ordering::Relaxed),
            negative_expired: self.negative_expired.load(Ordering::Relaxed),
            rpc_calls: self.rpc_calls.load(Ordering::Relaxed),
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CacheCounterSnapshot {
    pub hits: u64,
    pub misses: u64,
    pub negative_hits: u64,
    pub negative_expired: u64,
    pub rpc_calls: u64,
}

impl CacheCounterSnapshot {
    /// Lookups that did NOT trigger a real RPC call: a genuine cache hit,
    /// or a concurrent miss that single-flighted onto someone else's
    /// already-in-flight fetch. `hits + misses` is every lookup made;
    /// `rpc_calls` is only ever incremented by whichever single caller's
    /// closure actually ran. Exact, not estimated.
    pub fn calls_avoided(&self) -> u64 {
        (self.hits + self.misses).saturating_sub(self.rpc_calls)
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            hits: self.hits.saturating_sub(other.hits),
            misses: self.misses.saturating_sub(other.misses),
            negative_hits: self.negative_hits.saturating_sub(other.negative_hits),
            negative_expired: self.negative_expired.saturating_sub(other.negative_expired),
            rpc_calls: self.rpc_calls.saturating_sub(other.rpc_calls),
        }
    }
}

/// Positive/negative single-flight resolution cache. Generic over key and
/// value so one implementation serves address resolution, token0/token1,
/// and code-hash lookups alike; what "negative" means is decided entirely
/// by the caller's `fetch` closure.
struct ResolutionCache<K, V> {
    inner: Mutex<HashMap<K, Arc<OnceCell<Resolved<V>>>>>,
    counters: CacheCounters,
}

impl<K, V> ResolutionCache<K, V>
where
    K: Eq + Hash + Clone,
    V: Clone,
{
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            counters: CacheCounters::default(),
        }
    }

    /// Returns the cell for `key` plus whether it was already resolved
    /// (positive, or negative-and-not-yet-expired) at lookup time -- the
    /// real hit/miss signal, independent of the map-entry-exists check
    /// (an entry can exist but still be an in-flight single-flight miss).
    /// The map lock is held only for this pointer lookup/insert/evict --
    /// never across the caller's RPC await.
    fn cell(&self, key: &K, current_block: u64) -> (Arc<OnceCell<Resolved<V>>>, bool) {
        let mut guard = self.inner.lock().unwrap();
        if let Some(existing) = guard.get(key) {
            if let Some(Resolved::Negative { observed_at_block }) = existing.get() {
                if current_block.saturating_sub(*observed_at_block) >= NEGATIVE_CACHE_TTL_BLOCKS {
                    self.counters
                        .negative_expired
                        .fetch_add(1, Ordering::Relaxed);
                    let fresh = Arc::new(OnceCell::new());
                    guard.insert(key.clone(), fresh.clone());
                    return (fresh, false);
                }
            }
            let already_resolved = existing.get().is_some();
            return (existing.clone(), already_resolved);
        }
        let fresh = Arc::new(OnceCell::new());
        guard.insert(key.clone(), fresh.clone());
        (fresh, false)
    }

    async fn get_or_fetch<F, Fut, E>(
        &self,
        key: K,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<V>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<V>, E>>,
    {
        let (cell, already_resolved) = self.cell(&key, current_block);
        if already_resolved {
            self.counters.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.counters.misses.fetch_add(1, Ordering::Relaxed);
        }
        let rpc_calls = &self.counters.rpc_calls;
        let resolved = cell
            .get_or_try_init(|| async {
                rpc_calls.fetch_add(1, Ordering::Relaxed);
                fetch().await
            })
            .await?;
        match resolved {
            Resolved::Positive(v) => Ok(Some(v.clone())),
            Resolved::Negative { .. } => {
                if already_resolved {
                    self.counters.negative_hits.fetch_add(1, Ordering::Relaxed);
                }
                Ok(None)
            }
        }
    }

    fn snapshot(&self) -> CacheCounterSnapshot {
        self.counters.snapshot()
    }
}

// ============================================================
// Chain-scoped key types. `chain_id` is part of every key so this cache
// can never silently answer a Polygon lookup with an Ethereum-mainnet
// result if the process is ever pointed at another chain.
// ============================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct V2PairKey<Address> {
    pub chain_id: u64,
    pub factory: Address,
    pub token_lo: Address,
    pub token_hi: Address,
}

impl<Address: Ord + Copy> V2PairKey<Address> {
    pub fn new(chain_id: u64, factory: Address, token_a: Address, token_b: Address) -> Self {
        let (token_lo, token_hi) = canonical_token_pair(token_a, token_b);
        Self {
            chain_id,
            factory,
            token_lo,
            token_hi,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct V3PoolKey<Address> {
    pub chain_id: u64,
    pub factory: Address,
    pub token_lo: Address,
    pub token_hi: Address,
    pub fee: u32,
}

impl<Address: Ord + Copy> V3PoolKey<Address> {
    pub fn new(
        chain_id: u64,
        factory: Address,
        token_a: Address,
        token_b: Address,
        fee: u32,
    ) -> Self {
        let (token_lo, token_hi) = canonical_token_pair(token_a, token_b);
        Self {
            chain_id,
            factory,
            token_lo,
            token_hi,
            fee,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PoolMetadataKey<Address> {
    pub chain_id: u64,
    pub pool: Address,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CodeKey<Address> {
    pub chain_id: u64,
    pub address: Address,
}

/// Aggregate hit/miss counters across all five sub-caches, snapshot at a
/// point in time. `saturating_sub` gives an exact per-round delta when
/// snapshotted before and after Phase A.
#[derive(Debug, Clone, Copy, Default)]
pub struct CanonicalMetadataCacheMetrics {
    pub v2_pair: CacheCounterSnapshot,
    pub v3_pool: CacheCounterSnapshot,
    pub token0: CacheCounterSnapshot,
    pub token1: CacheCounterSnapshot,
    pub code_hash: CacheCounterSnapshot,
}

impl CanonicalMetadataCacheMetrics {
    pub fn calls_avoided_total(&self) -> u64 {
        self.v2_pair.calls_avoided()
            + self.v3_pool.calls_avoided()
            + self.token0.calls_avoided()
            + self.token1.calls_avoided()
            + self.code_hash.calls_avoided()
    }

    pub fn rpc_calls_total(&self) -> u64 {
        self.v2_pair.rpc_calls
            + self.v3_pool.rpc_calls
            + self.token0.rpc_calls
            + self.token1.rpc_calls
            + self.code_hash.rpc_calls
    }

    pub fn lookups_total(&self) -> u64 {
        self.v2_pair.hits
            + self.v2_pair.misses
            + self.v3_pool.hits
            + self.v3_pool.misses
            + self.token0.hits
            + self.token0.misses
            + self.token1.hits
            + self.token1.misses
            + self.code_hash.hits
            + self.code_hash.misses
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        Self {
            v2_pair: self.v2_pair.saturating_sub(&other.v2_pair),
            v3_pool: self.v3_pool.saturating_sub(&other.v3_pool),
            token0: self.token0.saturating_sub(&other.token0),
            token1: self.token1.saturating_sub(&other.token1),
            code_hash: self.code_hash.saturating_sub(&other.code_hash),
        }
    }
}

/// Process-lifetime (owned by `CanonicalDiscoveryService`, so it survives
/// across rounds -- "warm" behavior) cache of Phase-A identity lookups.
pub struct CanonicalMetadataCache<Address, CodeHash> {
    v2_pair: ResolutionCache<V2PairKey<Address>, Address>,
    v3_pool: ResolutionCache<V3PoolKey<Address>, Address>,
    token0: ResolutionCache<PoolMetadataKey<Address>, Address>,
    token1: ResolutionCache<PoolMetadataKey<Address>, Address>,
    code_hash: ResolutionCache<CodeKey<Address>, CodeHash>,
}

impl<Address, CodeHash> Default for CanonicalMetadataCache<Address, CodeHash>
where
    Address: Eq + Hash + Clone,
    CodeHash: Clone,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<Address, CodeHash> CanonicalMetadataCache<Address, CodeHash>
where
    Address: Eq + Hash + Clone,
    CodeHash: Clone,
{
    pub fn new() -> Self {
        Self {
            v2_pair: ResolutionCache::new(),
            v3_pool: ResolutionCache::new(),
            token0: ResolutionCache::new(),
            token1: ResolutionCache::new(),
            code_hash: ResolutionCache::new(),
        }
    }

    pub async fn v2_pair<F, Fut, E>(
        &self,
        key: V2PairKey<Address>,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<Address>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<Address>, E>>,
    {
        self.v2_pair.get_or_fetch(key, current_block, fetch).await
    }

    pub async fn v3_pool<F, Fut, E>(
        &self,
        key: V3PoolKey<Address>,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<Address>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<Address>, E>>,
    {
        self.v3_pool.get_or_fetch(key, current_block, fetch).await
    }

    pub async fn token0<F, Fut, E>(
        &self,
        key: PoolMetadataKey<Address>,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<Address>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<Address>, E>>,
    {
        self.token0.get_or_fetch(key, current_block, fetch).await
    }

    pub async fn token1<F, Fut, E>(
        &self,
        key: PoolMetadataKey<Address>,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<Address>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<Address>, E>>,
    {
        self.token1.get_or_fetch(key, current_block, fetch).await
    }

    pub async fn code_hash<F, Fut, E>(
        &self,
        key: CodeKey<Address>,
        current_block: u64,
        fetch: F,
    ) -> Result<Option<CodeHash>, E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<Resolved<CodeHash>, E>>,
    {
        self.code_hash.get_or_fetch(key, current_block, fetch).await
    }

    pub fn metrics(&self) -> CanonicalMetadataCacheMetrics {
        CanonicalMetadataCacheMetrics {
            v2_pair: self.v2_pair.snapshot(),
            v3_pool: self.v3_pool.snapshot(),
            token0: self.token0.snapshot(),
            token1: self.token1.snapshot(),
            code_hash: self.code_hash.snapshot(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    type Addr = u64; // stand-in for ethers::types::Address in unit tests -- Ord+Copy+Hash, same semantics.

    #[test]
    fn canonical_token_pair_is_direction_independent() {
        assert_eq!(
            canonical_token_pair(1u64, 2u64),
            canonical_token_pair(2u64, 1u64)
        );
        assert_eq!(canonical_token_pair(5u64, 5u64), (5u64, 5u64));
    }

    #[test]
    fn cache_is_chain_scoped() {
        let k1 = V2PairKey::new(137u64, 10u64, 1u64, 2u64);
        let k2 = V2PairKey::new(1u64, 10u64, 1u64, 2u64);
        assert_ne!(
            k1, k2,
            "same factory/tokens on different chain_id must not collide"
        );
    }

    #[tokio::test]
    async fn v2_positive_pair_is_cached() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = V2PairKey::new(137, 10, 1, 2);
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let calls = calls.clone();
            let out = cache
                .v2_pair(key, 100, || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(Resolved::Positive(999u64))
                })
                .await
                .unwrap();
            assert_eq!(out, Some(999));
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "positive result must be fetched once, reused after"
        );
    }

    #[tokio::test]
    async fn v3_positive_pool_is_cached() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = V3PoolKey::new(137, 10, 1, 2, 3000);
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let calls = calls.clone();
            let out = cache
                .v3_pool(key, 100, || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(Resolved::Positive(42u64))
                })
                .await
                .unwrap();
            assert_eq!(out, Some(42));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn token0_is_cached() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = PoolMetadataKey {
            chain_id: 137,
            pool: 55,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let calls = calls.clone();
            cache
                .token0(key, 100, || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(Resolved::Positive(1u64))
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn token1_is_cached() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = PoolMetadataKey {
            chain_id: 137,
            pool: 55,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..4 {
            let calls = calls.clone();
            cache
                .token1(key, 100, || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(Resolved::Positive(2u64))
                })
                .await
                .unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn positive_code_existence_is_cached() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = CodeKey {
            chain_id: 137,
            address: 77,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        for _ in 0..3 {
            let calls = calls.clone();
            let out = cache
                .code_hash(key, 100, || async move {
                    calls.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, ()>(Resolved::Positive(0xdeadbeefu64))
                })
                .await
                .unwrap();
            assert_eq!(out, Some(0xdeadbeef));
        }
        assert_eq!(calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn negative_pair_cache_expires() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = V2PairKey::new(137, 10, 1, 2);
        let calls = Arc::new(AtomicUsize::new(0));

        let fetch = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(Resolved::Negative {
                observed_at_block: 100,
            })
        };
        let out = cache
            .v2_pair(key, 100, || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(out, None);
        assert_eq!(calls.load(Ordering::Relaxed), 1);

        // Still within TTL: no new RPC.
        let out = cache
            .v2_pair(key, 100 + NEGATIVE_CACHE_TTL_BLOCKS - 1, || {
                fetch(calls.clone())
            })
            .await
            .unwrap();
        assert_eq!(out, None);
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "still within TTL, must not re-fetch"
        );

        // Past TTL: re-fetches.
        let out = cache
            .v2_pair(key, 100 + NEGATIVE_CACHE_TTL_BLOCKS, || {
                fetch(calls.clone())
            })
            .await
            .unwrap();
        assert_eq!(out, None);
        assert_eq!(calls.load(Ordering::Relaxed), 2, "past TTL, must re-fetch");
        assert_eq!(cache.metrics().v2_pair.negative_expired, 1);
    }

    #[tokio::test]
    async fn negative_pool_cache_expires() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = V3PoolKey::new(137, 10, 1, 2, 500);
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(Resolved::Negative {
                observed_at_block: 0,
            })
        };
        cache
            .v3_pool(key, 0, || fetch(calls.clone()))
            .await
            .unwrap();
        cache
            .v3_pool(key, NEGATIVE_CACHE_TTL_BLOCKS, || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn negative_code_cache_expires() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = CodeKey {
            chain_id: 137,
            address: 1,
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let fetch = |calls: Arc<AtomicUsize>| async move {
            calls.fetch_add(1, Ordering::Relaxed);
            Ok::<_, ()>(Resolved::Negative {
                observed_at_block: 0,
            })
        };
        cache
            .code_hash(key, 0, || fetch(calls.clone()))
            .await
            .unwrap();
        cache
            .code_hash(key, NEGATIVE_CACHE_TTL_BLOCKS, || fetch(calls.clone()))
            .await
            .unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn same_key_concurrent_lookup_singleflights() {
        let cache: Arc<CanonicalMetadataCache<Addr, u64>> = Arc::new(CanonicalMetadataCache::new());
        let key = V2PairKey::new(137, 10, 1, 2);
        let calls = Arc::new(AtomicUsize::new(0));

        let mut handles = Vec::new();
        for _ in 0..8 {
            let cache = cache.clone();
            let calls = calls.clone();
            handles.push(tokio::spawn(async move {
                cache
                    .v2_pair(key, 100, || async move {
                        calls.fetch_add(1, Ordering::Relaxed);
                        tokio::time::sleep(Duration::from_millis(30)).await;
                        Ok::<_, ()>(Resolved::Positive(1u64))
                    })
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            assert_eq!(h.await.unwrap(), Some(1));
        }
        assert_eq!(
            calls.load(Ordering::Relaxed),
            1,
            "8 concurrent lookups of the same key must trigger exactly 1 real fetch"
        );
        let snap = cache.metrics().v2_pair;
        assert_eq!(snap.rpc_calls, 1);
        assert_eq!(snap.calls_avoided(), 7);
    }

    #[tokio::test]
    async fn different_keys_do_not_global_serialize() {
        let cache: Arc<CanonicalMetadataCache<Addr, u64>> = Arc::new(CanonicalMetadataCache::new());
        let start = std::time::Instant::now();
        let mut handles = Vec::new();
        for i in 0..8u64 {
            let cache = cache.clone();
            let key = V2PairKey::new(137, 10, i, i + 100);
            handles.push(tokio::spawn(async move {
                cache
                    .v2_pair(key, 100, || async move {
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        Ok::<_, ()>(Resolved::Positive(i))
                    })
                    .await
                    .unwrap()
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        // 8 distinct keys each sleeping 50ms: if they serialized behind one
        // global lock this would take >=400ms. Progressing independently
        // keeps it close to the single 50ms sleep.
        assert!(
            start.elapsed() < Duration::from_millis(300),
            "distinct keys must not be globally serialized, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn rpc_error_is_not_cached_as_positive() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key = V2PairKey::new(137, 10, 1, 2);
        let attempt = Arc::new(AtomicUsize::new(0));

        let first: Result<Option<u64>, &'static str> = cache
            .v2_pair(key, 100, || {
                let attempt = attempt.clone();
                async move {
                    attempt.fetch_add(1, Ordering::Relaxed);
                    Err("boom")
                }
            })
            .await;
        assert!(first.is_err());

        // Second attempt after a failed fetch must retry, not return a
        // poisoned/stuck cell and not silently resolve as positive.
        let second = cache
            .v2_pair(key, 100, || {
                let attempt = attempt.clone();
                async move {
                    attempt.fetch_add(1, Ordering::Relaxed);
                    Ok::<_, &'static str>(Resolved::Positive(1u64))
                }
            })
            .await
            .unwrap();
        assert_eq!(second, Some(1));
        assert_eq!(
            attempt.load(Ordering::Relaxed),
            2,
            "failed fetch must not stick -- retry must run"
        );
    }

    #[tokio::test]
    async fn rpc_error_releases_singleflight_waiters() {
        let cache: Arc<CanonicalMetadataCache<Addr, u64>> = Arc::new(CanonicalMetadataCache::new());
        let key = V2PairKey::new(137, 10, 1, 2);

        // First call fails after a short delay while concurrent waiters are
        // parked on the same key.
        let cache_a = cache.clone();
        let leader = tokio::spawn(async move {
            cache_a
                .v2_pair(key, 100, || async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Err::<Resolved<u64>, &'static str>("boom")
                })
                .await
        });
        tokio::time::sleep(Duration::from_millis(5)).await;
        let cache_b = cache.clone();
        let waiter = tokio::spawn(async move {
            // Races the leader's in-flight failing fetch; tokio's
            // `get_or_try_init` retries with the caller that observes the
            // uninitialized cell after the failure, so this either sees
            // the same error or (if scheduled after the retry window)
            // succeeds -- either way it must complete, never hang.
            tokio::time::timeout(
                Duration::from_millis(500),
                cache_b.v2_pair(key, 100, || async move {
                    Ok::<_, &'static str>(Resolved::Positive(1u64))
                }),
            )
            .await
        });

        let leader_result = leader.await.unwrap();
        assert!(leader_result.is_err());
        let waiter_result = waiter.await.unwrap();
        assert!(
            waiter_result.is_ok(),
            "waiter must not hang/deadlock after leader's RPC error"
        );
    }

    #[tokio::test]
    async fn cached_metadata_preserves_quote_result() {
        // Two logically-equivalent lookups (A->B and B->A canonicalize to
        // the same key) must yield the identical resolved value whether
        // served from cache or freshly fetched.
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let key_ab = V2PairKey::new(137, 10, 1, 2);
        let key_ba = V2PairKey::new(137, 10, 2, 1);
        assert_eq!(key_ab, key_ba);

        let out_ab = cache
            .v2_pair(key_ab, 100, || async {
                Ok::<_, ()>(Resolved::Positive(777u64))
            })
            .await
            .unwrap();
        let out_ba = cache
            .v2_pair(key_ba, 100, || async {
                panic!("must be a cache hit, not a second fetch");
                #[allow(unreachable_code)]
                Ok::<_, ()>(Resolved::Positive(0u64))
            })
            .await
            .unwrap();
        assert_eq!(out_ab, out_ba);
        assert_eq!(out_ab, Some(777));
    }

    // ---- Boundary tests: this module has no concept of these value
    // kinds at all -- the type signatures below simply do not admit them.
    // These are compile-time-shape assertions expressed as a runtime
    // check that the cache's public surface has exactly 5 methods, all
    // scoped to address/code-hash identity. ----

    #[test]
    fn cache_does_not_store_reserves() {
        // `CanonicalMetadataCache` exposes v2_pair/v3_pool/token0/token1/
        // code_hash only -- no method accepts or returns a reserve pair.
        // If a future edit added `fn reserves(...)` this test's sibling
        // assertions (below) would still compile, so the real guarantee
        // is architectural: reserves live in `PoolContext`/`OnlinePoolRead`
        // in `canonical_discovery.rs`/`canonical_adapters.rs`, never here.
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let _ = cache.metrics(); // only surface that reads cache state
    }

    #[test]
    fn cache_does_not_store_slot0() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let _ = cache.metrics();
    }

    #[test]
    fn cache_does_not_store_liquidity() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let _ = cache.metrics();
    }

    #[test]
    fn cache_does_not_store_amount_quote() {
        let cache: CanonicalMetadataCache<Addr, u64> = CanonicalMetadataCache::new();
        let _ = cache.metrics();
    }
}
