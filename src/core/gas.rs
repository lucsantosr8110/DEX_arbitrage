// ============================================================
// src/core/gas.rs — v4.8.5-POLYGON-ORACLE
// ✅ Integração com Gas Oracle oficial da Polygon
// ✅ TTL 30s configurável + fallback RPC
// ✅ Log "GasOracleSync" detalhado
// ✅ Mantém hot-reload e microprofit tuning
// ============================================================

use anyhow::Result;
use ethers::{
    prelude::*,
    types::{BlockId, BlockNumber, Eip1559TransactionRequest},
};
use reqwest::Client;
use serde::Deserialize;
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, info, warn};

use crate::config::Config;

// ============================================================
// 🧩 Estrutura de Cache Interno
// ============================================================

#[derive(Clone, Debug)]
struct GasCacheEntry {
    max_fee: U256,
    priority_fee: U256,
    _base_fee: U256,
    timestamp: Instant,
}

// ============================================================
// 🧠 Estruturas para o Polygon Gas Oracle
// ============================================================

#[derive(Deserialize, Debug, Clone)]
struct PolygonGasTier {
    #[serde(rename = "maxFee")]
    max_fee: f64,
    #[serde(rename = "maxPriorityFee")]
    max_priority_fee: f64,
}

#[derive(Deserialize, Debug, Clone)]
struct PolygonGasOracle {
    safeLow: PolygonGasTier,
    standard: PolygonGasTier,
    fast: PolygonGasTier,
}

// ============================================================
// ⚙️ Estimador Dinâmico de Gás
// ============================================================

pub struct GasEstimator<M> {
    client: Arc<M>,
    config: Arc<Mutex<Config>>,
    gas_cache: Arc<RwLock<HashMap<String, GasCacheEntry>>>,
    base_fee_cache: Arc<RwLock<Option<(U256, Instant)>>>,
    oracle_cache: Arc<RwLock<Option<(PolygonGasOracle, Instant)>>>,
    http: Client,
}

impl<M> GasEstimator<M>
where
    M: Middleware + 'static,
{
    pub fn new(client: Arc<M>, config: Arc<Mutex<Config>>) -> Self {
        Self {
            client,
            config,
            gas_cache: Arc::new(RwLock::new(HashMap::new())),
            base_fee_cache: Arc::new(RwLock::new(None)),
            oracle_cache: Arc::new(RwLock::new(None)),
            http: Client::builder().timeout(Duration::from_secs(3)).build().unwrap(),
        }
    }

    // ============================================================
    // ⛽ População Dinâmica de Gás
    // ============================================================
    pub async fn populate_dynamic_gas(
        &self,
        tx: &mut Eip1559TransactionRequest,
    ) -> Result<(U256, U256)> {
        let cfg = self.config.lock().await.clone();
        let gas = &cfg.gas;

        let ttl = Duration::from_secs(gas.cache_ttl.max(10));
        let cache_key = "polygon_dynamic_gas".to_string();

        // Usa o cache local se ainda estiver válido
        if let Some(entry) = self.get_cached_gas(&cache_key, ttl).await {
            tx.max_fee_per_gas = Some(entry.max_fee);
            tx.max_priority_fee_per_gas = Some(entry.priority_fee);
            return Ok((entry.max_fee, entry.priority_fee));
        }

        // ============================================================
        // 🔍 Tenta obter dados do Gas Oracle oficial
        // ============================================================
        if gas.use_polygon_oracle {
            if let Ok(Some((max_fee_gwei, prio_gwei))) = self.fetch_polygon_oracle_ttl(ttl).await {
                let base_fee = gwei((max_fee_gwei - prio_gwei).max(0.0) as u64);
                let max_fee = gwei(max_fee_gwei as u64);
                let priority_fee = gwei(prio_gwei as u64);

                self.set_cached_gas(&cache_key, max_fee, priority_fee, base_fee).await;
                info!(
                    "🔗 [GasOracleSync] Oracle → Base: {:.2} | Priority: {:.2} | Max: {:.2} Gwei",
                    max_fee_gwei - prio_gwei,
                    prio_gwei,
                    max_fee_gwei
                );

                tx.max_fee_per_gas = Some(max_fee);
                tx.max_priority_fee_per_gas = Some(priority_fee);
                return Ok((max_fee, priority_fee));
            } else {
                warn!("⚠️ [GasOracleSync] Falha no Oracle — usando fallback RPC");
            }
        }

        // ============================================================
        // 🧩 Fallback via RPC BaseFee
        // ============================================================
        let base_fee =
            self.get_cached_base_fee(ttl).await?.unwrap_or(gwei(gas.default_gas_price_gwei as u64));

        let priority_fee = self.calculate_dynamic_priority_fee(&cfg, base_fee).await?;
        let max_fee = self.calculate_dynamic_max_fee(&cfg, base_fee, priority_fee).await?;

        self.set_cached_gas(&cache_key, max_fee, priority_fee, base_fee)
            .await;

        info!(
            "⛽ GAS Dinâmico RPC → Base: {:.2} | Priority: {:.2} | Max: {:.2} Gwei",
            u256_to_f64(base_fee, 9),
            u256_to_f64(priority_fee, 9),
            u256_to_f64(max_fee, 9)
        );

        tx.max_fee_per_gas = Some(max_fee);
        tx.max_priority_fee_per_gas = Some(priority_fee);
        Ok((max_fee, priority_fee))
    }

    // ============================================================
    // 🔗 Consulta ao Polygon Gas Oracle com cache (TTL 30s)
    // ============================================================
    async fn fetch_polygon_oracle_ttl(&self, ttl: Duration) -> Result<Option<(f64, f64)>> {
        {
            let cache = self.oracle_cache.read().await;
            if let Some((oracle, ts)) = &*cache {
                if ts.elapsed() < ttl {
                    let s = &oracle.standard;
                    return Ok(Some((s.max_fee, s.max_priority_fee)));
                }
            }
        }

        match self.http.get("https://gasstation.polygon.technology/v2").send().await {
            Ok(resp) => {
                if resp.status().is_success() {
                    let oracle: PolygonGasOracle = resp.json().await?;
                    let s = &oracle.standard;
                    let mut cache = self.oracle_cache.write().await;
                    *cache = Some((oracle.clone(), Instant::now()));
                    Ok(Some((s.max_fee, s.max_priority_fee)))
                } else {
                    warn!("⚠️ Polygon Gas Oracle HTTP {}", resp.status());
                    Ok(None)
                }
            }
            Err(e) => {
                warn!("⚠️ Erro ao acessar Polygon Gas Oracle: {}", e);
                Ok(None)
            }
        }
    }

    // ============================================================
    // 🧮 Cálculos Dinâmicos (fallback)
    // ============================================================
    async fn calculate_dynamic_priority_fee(
        &self,
        cfg: &Config,
        base_fee: U256,
    ) -> Result<U256> {
        let base_fee_gwei = u256_to_f64(base_fee, 9);
        let mut priority = cfg.gas.priority_gwei;
        let max_priority = cfg.gas.max_priority_gwei;
        let min_priority = cfg.gas.min_priority_gwei;

        if cfg.gas.optimization.auto_adjust {
            if base_fee_gwei < 25.0 {
                priority *= 0.8;
            } else if base_fee_gwei > 50.0 {
                priority *= 1.2;
            }
        }

        priority = priority.clamp(min_priority, max_priority);
        Ok(gwei(priority as u64))
    }

    async fn calculate_dynamic_max_fee(
        &self,
        cfg: &Config,
        base_fee: U256,
        priority_fee: U256,
    ) -> Result<U256> {
        let base_fee_gwei = u256_to_f64(base_fee, 9);
        let priority_gwei = u256_to_f64(priority_fee, 9);
        let multiplier = cfg.gas.dynamic_multiplier;

        let mut max_fee_gwei = (base_fee_gwei * multiplier) + priority_gwei;
        if max_fee_gwei > cfg.gas.max_gwei as f64 {
            max_fee_gwei = cfg.gas.max_gwei as f64;
        }

        Ok(gwei(max_fee_gwei as u64))
    }

    // ============================================================
    // 💵 Estimativa de custo
    // ============================================================
    pub async fn estimate_arbitrage_gas_usd(&self) -> Result<f64> {
        let cfg = self.config.lock().await.clone();
        let gas_cfg = &cfg.gas;

        let ttl = Duration::from_secs(gas_cfg.cache_ttl.max(10));
        let base_fee = self
            .get_cached_base_fee(ttl)
            .await?
            .unwrap_or(gwei(gas_cfg.default_gas_price_gwei as u64));

        let (_, priority_fee) =
            self.populate_dynamic_gas(&mut Eip1559TransactionRequest::default())
                .await?;

        let base_fee_gwei = u256_to_f64(base_fee, 9);
        let priority_gwei = u256_to_f64(priority_fee, 9);
        let eff = base_fee_gwei * 1.05 + priority_gwei;

        let gas_units = if gas_cfg.default_gas_limit == 0 {
            gas_cfg.max_gas_limit as f64
        } else {
            gas_cfg.default_gas_limit as f64
        };

        let matic_price = gas_cfg.eth_price_usd;
        let cost = gas_units * (eff * 1e-9) * matic_price;

        info!(
            "⛽ [GasEstimator] base={:.2} | prio={:.2} | eff={:.2} | custo=${:.6}",
            base_fee_gwei, priority_gwei, eff, cost
        );

        Ok(cost)
    }

    // ============================================================
    // 🔁 Cache
    // ============================================================
    async fn get_cached_base_fee(&self, ttl: Duration) -> Result<Option<U256>> {
        {
            let cache = self.base_fee_cache.read().await;
            if let Some((cached, ts)) = *cache {
                if ts.elapsed() < ttl {
                    return Ok(Some(cached));
                }
            }
        }

        let base_fee = latest_base_fee(&self.client).await?;
        let mut cache = self.base_fee_cache.write().await;
        if let Some(bf) = base_fee {
            *cache = Some((bf, Instant::now()));
        }
        Ok(base_fee)
    }

    async fn get_cached_gas(&self, key: &str, ttl: Duration) -> Option<GasCacheEntry> {
        let cache = self.gas_cache.read().await;
        cache.get(key).and_then(|e| {
            if e.timestamp.elapsed() < ttl {
                Some(e.clone())
            } else {
                None
            }
        })
    }

    async fn set_cached_gas(&self, key: &str, max_fee: U256, priority_fee: U256, base_fee: U256) {
        let entry = GasCacheEntry {
            max_fee,
            priority_fee,
            _base_fee: base_fee,
            timestamp: Instant::now(),
        };
        let mut cache = self.gas_cache.write().await;
        cache.insert(key.to_string(), entry);
    }
}

// ============================================================
// 🔧 Funções utilitárias
// ============================================================

fn gwei(n: u64) -> U256 {
    U256::from(n) * U256::exp10(9)
}

pub fn u256_to_f64(value: U256, decimals: u32) -> f64 {
    let divisor = U256::exp10(decimals as usize);
    let integer = value / divisor;
    let fractional = value % divisor;
    integer.as_u64() as f64 + (fractional.as_u64() as f64 / 10f64.powi(decimals as i32))
}

async fn latest_base_fee<M: Middleware>(client: &Arc<M>) -> Result<Option<U256>> {
    match client.get_block(BlockId::Number(BlockNumber::Latest)).await {
        Ok(Some(block)) => Ok(block.base_fee_per_gas),
        Ok(None) => {
            warn!("⚠️ Nenhum bloco recente encontrado");
            Ok(None)
        }
        Err(e) => {
            warn!("❌ Falha ao obter base fee: {}", e);
            Ok(None)
        }
    }
}

// ============================================================
// ♻️ Clone Seguro
// ============================================================

impl<M> Clone for GasEstimator<M> {
    fn clone(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: self.config.clone(),
            gas_cache: self.gas_cache.clone(),
            base_fee_cache: self.base_fee_cache.clone(),
            oracle_cache: self.oracle_cache.clone(),
            http: self.http.clone(),
        }
    }
}
