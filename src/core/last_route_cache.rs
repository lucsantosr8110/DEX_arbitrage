// ============================================================
// src/core/last_route_cache.rs — Fast-path re-eval cache
// ============================================================
//
// Cacheia última rota economicamente positiva. Se anchor moveu <5 blocks
// e cache tem <30s, o round seguinte re-evalua economics apenas (sem
// re-discovery full de 70-90s). Ganho: rounds back-to-back winners
// (observados em #53/54, #653/654/655) saltam para ~5s por round.

use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::trace;

/// Rota cacheada. `cycle_rate` é o `total_rate` do produto das pernas;
/// `gross_usd` é o lucro bruto projetado em USD; `net_usd` é o net já
/// com custos. `venues` e `path` são para log/dashboard.
#[derive(Debug, Clone)]
pub struct CachedRoute {
    pub anchor_block: u64,
    pub computed_at: Instant,
    pub cycle_rate: f64,
    pub gross_usd: f64,
    pub net_usd: f64,
    pub venues: String,
    pub path: String,
    pub trade_size_usd: f64,
}

#[derive(Default)]
struct Inner {
    last: Option<CachedRoute>,
    hits: u64,
    misses: u64,
}

static CACHE: once_cell::sync::Lazy<Arc<RwLock<Inner>>> =
    once_cell::sync::Lazy::new(|| Arc::new(RwLock::new(Inner::default())));

/// TTL do cache: 30s. Acima disso, edge já evaporou.
const TTL: Duration = Duration::from_secs(30);
/// Anchor tolerance: 5 blocks (~10s em Polygon). Acima disso, mundo mudou.
const ANCHOR_TOLERANCE: u64 = 5;

/// Tenta usar o cache. Retorna `Some(cached)` se fresco o suficiente,
/// `None` caso contrário. Atualiza contadores de hit/miss para telemetria.
pub async fn try_get(current_anchor: u64) -> Option<CachedRoute> {
    let mut cache = CACHE.write().await;
    let Some(ref last) = cache.last else {
        cache.misses += 1;
        return None;
    };
    let anchor_delta = current_anchor.saturating_sub(last.anchor_block);
    let age = last.computed_at.elapsed();
    let anchor_ok = anchor_delta <= ANCHOR_TOLERANCE;
    let time_ok = age < TTL;
    if anchor_ok && time_ok {
        let snapshot = last.clone();
        cache.hits += 1;
        trace!(target: "last_route_cache", anchor = current_anchor, "hit (anchor Δ={}, age={:?})", anchor_delta, age);
        Some(snapshot)
    } else {
        cache.misses += 1;
        None
    }
}

/// Salva rota no cache. Chamado pelo main loop após cada round completo.
pub async fn put(route: CachedRoute) {
    let mut cache = CACHE.write().await;
    cache.last = Some(route);
}

pub async fn invalidate() {
    let mut cache = CACHE.write().await;
    cache.last = None;
}

/// Telemetria: (hits, misses, last_age_secs).
pub async fn stats() -> (u64, u64, Option<f64>) {
    let cache = CACHE.read().await;
    let age = cache.last.as_ref().map(|r| r.computed_at.elapsed().as_secs_f64());
    (cache.hits, cache.misses, age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn miss_when_empty() {
        invalidate().await;
        assert!(try_get(100).await.is_none());
    }

    #[tokio::test]
    async fn hit_when_fresh() {
        invalidate().await;
        put(CachedRoute {
            anchor_block: 100,
            computed_at: Instant::now(),
            cycle_rate: 1.01,
            gross_usd: 1.0,
            net_usd: 0.5,
            venues: "A/B".into(),
            path: "X-Y-X".into(),
            trade_size_usd: 100.0,
        }).await;
        let got = try_get(103).await.unwrap();
        assert!((got.cycle_rate - 1.01).abs() < 1e-9);
    }

    #[tokio::test]
    async fn miss_when_anchor_too_far() {
        invalidate().await;
        put(CachedRoute {
            anchor_block: 100,
            computed_at: Instant::now(),
            cycle_rate: 1.0,
            gross_usd: 0.0,
            net_usd: 0.0,
            venues: String::new(),
            path: String::new(),
            trade_size_usd: 0.0,
        }).await;
        // anchor 6 blocks ahead (acima da tolerância de 5)
        assert!(try_get(106).await.is_none());
    }
}
