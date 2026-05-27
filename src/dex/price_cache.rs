// ================================================================
// src/dex/price_cache.rs — v3.9.1 (CORRIGIDO E OTIMIZADO)
// ================================================================

use ethers::types::{Address, U256};
use prometheus::{register_int_counter, register_int_gauge, IntCounter, IntGauge};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ================================================================
// 🧠 Estrutura principal
// ================================================================

#[derive(Clone, Debug)]
struct CacheEntry {
    value: U256,
    timestamp: Instant,
}

#[derive(Clone)]
pub struct PriceCache {
    inner: Arc<RwLock<HashMap<(String, Address, Address), CacheEntry>>>,
    ttl: Duration,
    metrics: CacheMetrics,
}

// ================================================================
// 📊 Métricas Prometheus
// ================================================================

#[derive(Clone)]
struct CacheMetrics {
    hits: IntCounter,
    misses: IntCounter,
    size: IntGauge,
}

impl CacheMetrics {
    fn new() -> Self {
        Self {
            hits: register_int_counter!("dex_cache_hits_total", "Total de hits no cache de preços")
                .unwrap_or_else(|_| IntCounter::new("dex_cache_hits_fallback", "hits").unwrap()),
            misses: register_int_counter!(
                "dex_cache_misses_total",
                "Total de misses no cache de preços"
            )
            .unwrap_or_else(|_| IntCounter::new("dex_cache_misses_fallback", "misses").unwrap()),
            size: register_int_gauge!("dex_cache_size", "Número atual de entradas no cache")
                .unwrap_or_else(|_| IntGauge::new("dex_cache_size_fallback", "size").unwrap()),
        }
    }
}

// ================================================================
// 🧩 Implementação do Cache
// ================================================================

impl PriceCache {
    /// Cria um novo cache com TTL padrão
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            ttl,
            metrics: CacheMetrics::new(),
        }
    }

    /// Obtém valor do cache (retorna `Some(U256)` se válido)
    pub async fn get(&self, dex: &str, token_in: Address, token_out: Address) -> Option<U256> {
        let cache = self.inner.read().await;
        let key = (dex.to_string(), token_in, token_out);

        if let Some(entry) = cache.get(&key) {
            if entry.timestamp.elapsed() < self.ttl {
                self.metrics.hits.inc();
                debug!(
                    "💾 Cache HIT [{}][{}→{}]",
                    dex,
                    short_address(token_in),
                    short_address(token_out)
                );
                return Some(entry.value);
            }
        }

        self.metrics.misses.inc();
        None
    }

    /// Define novo valor no cache
    pub async fn set(&self, dex: &str, token_in: Address, token_out: Address, value: U256) {
        let mut cache = self.inner.write().await;
        let key = (dex.to_string(), token_in, token_out);
        cache.insert(
            key,
            CacheEntry {
                value,
                timestamp: Instant::now(),
            },
        );
        self.metrics.size.set(cache.len() as i64);
    }

    /// Remove entradas expiradas
    pub async fn prune_expired(&self) -> usize {
        let mut cache = self.inner.write().await;
        let before = cache.len();
        cache.retain(|_, entry| entry.timestamp.elapsed() < self.ttl);
        let after = cache.len();
        let removed = before.saturating_sub(after);

        self.metrics.size.set(after as i64);

        if removed > 0 {
            debug!("🧹 Cache limpo: {} entradas expiradas removidas", removed);
        }

        removed
    }

    /// Retorna número atual de entradas
    pub async fn len(&self) -> usize {
        self.inner.read().await.len()
    }

    /// ✅ Compatibilidade com DexManager
    pub async fn get_size(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Verifica se o cache está vazio
    pub async fn is_empty(&self) -> bool {
        self.inner.read().await.is_empty()
    }

    /// Limpa completamente o cache
    pub async fn clear(&self) -> usize {
        let mut cache = self.inner.write().await;
        let size = cache.len();
        cache.clear();
        self.metrics.size.set(0);
        size
    }

    /// Estatísticas do cache
    pub async fn get_stats(&self) -> CacheStats {
        let cache = self.inner.read().await;
        let mut valid_entries = 0;
        let mut expired_entries = 0;

        for entry in cache.values() {
            if entry.timestamp.elapsed() < self.ttl {
                valid_entries += 1;
            } else {
                expired_entries += 1;
            }
        }

        CacheStats {
            total_entries: cache.len(),
            valid_entries,
            expired_entries,
            hit_count: self.metrics.hits.get(),
            miss_count: self.metrics.misses.get(),
        }
    }

    // ============================================================
    // ⚡ MÉTODO CORRIGIDO: warm_up() - AGORA FUNCIONAL
    // ============================================================
    /// Realiza o "pré-carregamento" de pares importantes no cache
    pub async fn warm_up(&self, pairs: Vec<(String, Address, Address, U256)>) -> usize {
        if pairs.is_empty() {
            warn!("⚠️ [Cache] warm_up chamado com lista vazia");
            return 0;
        }

        let mut cache = self.inner.write().await;
        let now = Instant::now();
        let mut loaded = 0;

        for (dex, token_in, token_out, price) in pairs {
            let key = (dex, token_in, token_out);
            cache.insert(
                key,
                CacheEntry {
                    value: price,
                    timestamp: now,
                },
            );
            loaded += 1;
        }

        self.metrics.size.set(cache.len() as i64);
        info!(
            "🔥 Cache warm-up concluído: {} pares pré-carregados",
            loaded
        );
        
        loaded
    }

    /// 🔄 NOVO: Atualiza múltiplos pares de uma vez (mais eficiente)
    pub async fn bulk_update(&self, updates: Vec<(&str, Address, Address, U256)>) {
        let mut cache = self.inner.write().await;
        let now = Instant::now();

        for (dex, token_in, token_out, price) in updates {
            let key = (dex.to_string(), token_in, token_out);
            cache.insert(key, CacheEntry { value: price, timestamp: now });
        }

        self.metrics.size.set(cache.len() as i64);
    }

    /// 🎯 NOVO: Obtém todos os pares válidos para um DEX específico
    pub async fn get_dex_prices(&self, dex: &str) -> HashMap<(Address, Address), U256> {
        let cache = self.inner.read().await;
        let mut result = HashMap::new();
        let now = Instant::now();

        for ((dex_name, token_in, token_out), entry) in cache.iter() {
            if dex_name == dex && now.duration_since(entry.timestamp) < self.ttl {
                result.insert((*token_in, *token_out), entry.value);
            }
        }

        result
    }
}

// ================================================================
// 📊 Estrutura de estatísticas do cache
// ================================================================

#[derive(Debug, Clone)]
pub struct CacheStats {
    pub total_entries: usize,
    pub valid_entries: usize,
    pub expired_entries: usize,
    pub hit_count: u64,
    pub miss_count: u64,
}

impl CacheStats {
    pub fn hit_rate(&self) -> f64 {
        let total = self.hit_count + self.miss_count;
        if total > 0 {
            self.hit_count as f64 / total as f64
        } else {
            0.0
        }
    }

    pub fn print_summary(&self) {
        info!("📊 Cache Stats:");
        info!(
            "  • Entradas: {}/{} válidas ({} expiradas)",
            self.valid_entries, self.total_entries, self.expired_entries
        );
        info!(
            "  • Hit Rate: {:.1}% ({}/{} requests)",
            self.hit_rate() * 100.0,
            self.hit_count,
            self.hit_count + self.miss_count
        );
    }
}

// ================================================================
// 🔧 UTILITÁRIOS
// ================================================================

fn short_address(addr: Address) -> String {
    let hex = format!("{:?}", addr);
    if hex.len() > 10 {
        format!("{}...{}", &hex[0..6], &hex[hex.len() - 4..])
    } else {
        hex
    }
}

// ================================================================
// 🧪 TESTES ATUALIZADOS
// ================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;

    fn dummy_address(seed: u8) -> Address {
        let mut bytes = [0u8; 20];
        bytes[19] = seed;
        Address::from(bytes)
    }

    #[tokio::test]
    async fn test_set_get() {
        let cache = PriceCache::new(Duration::from_secs(2));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);
        let val = U256::from(1000);

        cache.set("UniswapV2", addr_a, addr_b, val).await;
        let result = cache.get("UniswapV2", addr_a, addr_b).await;
        assert_eq!(result, Some(val));
    }

    #[tokio::test]
    async fn test_expiry() {
        let cache = PriceCache::new(Duration::from_millis(10));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);
        let val = U256::from(1000);

        cache.set("SushiSwap", addr_a, addr_b, val).await;
        tokio::time::sleep(Duration::from_millis(15)).await;
        assert!(cache.get("SushiSwap", addr_a, addr_b).await.is_none());
    }

    #[tokio::test]
    async fn test_prune() {
        let cache = PriceCache::new(Duration::from_millis(10));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);

        cache
            .set("QuickSwap", addr_a, addr_b, U256::from(1000))
            .await;
        tokio::time::sleep(Duration::from_millis(15)).await;

        let removed = cache.prune_expired().await;
        assert_eq!(removed, 1);
    }

    #[tokio::test]
    async fn test_get_size() {
        let cache = PriceCache::new(Duration::from_secs(10));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);

        assert_eq!(cache.get_size().await, 0);

        cache
            .set("UniswapV3", addr_a, addr_b, U256::from(1000))
            .await;
        assert_eq!(cache.get_size().await, 1);

        cache
            .set("SushiSwap", addr_a, addr_b, U256::from(2000))
            .await;
        assert_eq!(cache.get_size().await, 2);
    }

    #[tokio::test]
    async fn test_warm_up_functional() {
        let cache = PriceCache::new(Duration::from_secs(10));
        let weth = dummy_address(1);
        let usdc = dummy_address(2);
        let usdt = dummy_address(3);
        
        let pairs = vec![
            ("UniswapV3".to_string(), weth, usdc, U256::from(2000_000000u64)),
            ("SushiSwap".to_string(), weth, usdt, U256::from(1990_000000u64)),
        ];
        
        cache.warm_up(pairs).await;
        assert_eq!(cache.get_size().await, 2);
        
        // Verifica se os valores foram carregados corretamente
        assert!(cache.get("UniswapV3", weth, usdc).await.is_some());
        assert!(cache.get("SushiSwap", weth, usdt).await.is_some());
    }

    #[tokio::test]
    async fn test_bulk_update() {
        let cache = PriceCache::new(Duration::from_secs(10));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);
        let addr_c = dummy_address(3);

        let updates = vec![
            ("UniswapV2", addr_a, addr_b, U256::from(1000)),
            ("SushiSwap", addr_a, addr_c, U256::from(2000)),
            ("QuickSwap", addr_b, addr_c, U256::from(3000)),
        ];

        cache.bulk_update(updates).await;
        assert_eq!(cache.get_size().await, 3);
    }

    #[tokio::test]
    async fn test_get_dex_prices() {
        let cache = PriceCache::new(Duration::from_secs(10));
        let addr_a = dummy_address(1);
        let addr_b = dummy_address(2);
        let addr_c = dummy_address(3);

        cache.set("UniswapV3", addr_a, addr_b, U256::from(1000)).await;
        cache.set("UniswapV3", addr_a, addr_c, U256::from(2000)).await;
        cache.set("SushiSwap", addr_a, addr_b, U256::from(1500)).await;

        let uniswap_prices = cache.get_dex_prices("UniswapV3").await;
        assert_eq!(uniswap_prices.len(), 2);
        assert_eq!(uniswap_prices.get(&(addr_a, addr_b)), Some(&U256::from(1000)));
    }
}