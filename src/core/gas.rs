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
use tracing::{info, warn};

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
    /// EWMA de gas real/estimado, aprendido de receipts confirmados.
    gas_unit_multiplier: Arc<RwLock<f64>>,
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
            gas_unit_multiplier: Arc::new(RwLock::new(1.0)),
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
                let base_fee = gwei_f64((max_fee_gwei - prio_gwei).max(0.0));
                let max_fee = gwei_f64(max_fee_gwei);
                let priority_fee = gwei_f64(prio_gwei);

                self.set_cached_gas(&cache_key, max_fee, priority_fee, base_fee).await;
                // M7: popular base_fee_cache com o base_fee derivado do oracle
                // (max_fee − priority). Antes ia p/ campo `_base_fee` (unused) e o
                // custo fazia SEGUNDA RPC `get_cached_base_fee`. Agora quem ler
                // base_fee em seguida pega do cache (0 RPCs extra).
                {
                    let mut cache = self.base_fee_cache.write().await;
                    *cache = Some((base_fee, Instant::now()));
                }
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

        // M20: URL do oracle parametrizada por config.gas.stations.polygon_oracle_url.
        // Antes hardcoded `https://gasstation.polygon.technology/v2` — em outra
        // chain o bot consultaria oracle errado silenciosamente.
        let url = {
            let cfg = self.config.lock().await;
            cfg.gas.stations.polygon_oracle_url.clone()
        };
        match self.http.get(&url).send().await {
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
        Ok(gwei_f64(priority))
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

        Ok(gwei_f64(max_fee_gwei))
    }

    // ============================================================
    // 💵 Estimativa de custo
    // ============================================================
    /// Custo de gás para uma rota de `n_hops` pernas. (Audit M4)
    ///
    /// Antes `gas_units` era fixo (`estimated_gas_units`, 400k) independente de
    /// hops — rotas 4-5 hops queimam > 400k (V2 ≈ 80-110k/hop, V3 ≈ 150-180k/hop,
    /// +overhead Aave). Custo subestimado em rotas longas → hurdle baixo →
    /// executa opps que dão prejuízo. Agora escala: `base_per_hop × n_hops +
    /// flashloan_overhead`, derivando `base_per_hop` de `estimated_gas_units`
    /// (interpretado como rota de 3 hops) menos `flashloan.gas_overhead`.
    pub async fn estimate_gas_usd_for_hops(&self, n_hops: usize) -> Result<f64> {
        let cfg = self.config.lock().await.clone();
        let gas_cfg = &cfg.gas;

        let ttl = Duration::from_secs(gas_cfg.cache_ttl.max(10));

        // M7: populate_dynamic_gas pode popular base_fee_cache via oracle, evitando
        // a segunda RPC `get_cached_base_fee` que antes sempre rodava.
        let (_, priority_fee) =
            self.populate_dynamic_gas(&mut Eip1559TransactionRequest::default())
                .await?;
        let base_fee = self
            .get_cached_base_fee(ttl)
            .await?
            .unwrap_or(gwei(gas_cfg.default_gas_price_gwei as u64));

        let base_fee_gwei = u256_to_f64(base_fee, 9);
        let priority_gwei = u256_to_f64(priority_fee, 9);
        let eff = base_fee_gwei * 1.05 + priority_gwei;

        let baseline_units = Self::gas_units_for_hops(gas_cfg, &cfg, n_hops);
        let multiplier = *self.gas_unit_multiplier.read().await;
        let gas_units = baseline_units * multiplier;

        // Preço do POL (token de gás da Polygon) via Coingecko com cache de 2 min.
        let matic_price = crate::infra::price_feed::PRICE_FEED
            .get_price("WMATIC")
            .await
            .unwrap_or_else(|_| crate::infra::price_feed::CachedPriceFeed::fallback_price("WMATIC"));
        let cost = gas_units * (eff * 1e-9) * matic_price;

        // Finder guarda referência canônica de 3 hops; publicar uma rota de
        // 2/4 hops e depois escalá-la de novo distorce custo por 33%.
        if n_hops == 3 {
            crate::core::economics::publish_live_gas_usd(cost);
        }

        info!(
            "⛽ [GasEstimator] base={:.2} | prio={:.2} | eff={:.2} | hops={} | units={:.0} | custo=${:.6}",
            base_fee_gwei, priority_gwei, eff, n_hops, gas_units, cost
        );

        Ok(cost)
    }

    /// Backward-compat: custo para rota de 3 hops (referência do
    /// `estimated_gas_units`). Callers sem `n_hops` explícito caem aqui.
    pub async fn estimate_arbitrage_gas_usd(&self) -> Result<f64> {
        self.estimate_gas_usd_for_hops(3).await
    }

    /// `gas_units` escalado por hops (M4). Deriva `base_per_hop` de
    /// `estimated_gas_units` (rota de 3 hops) e `flashloan.gas_overhead`. Sem
    /// `estimated_gas_units`, cai em `default_gas_limit`/`max_gas_limit` (teto).
    fn gas_units_for_hops(gas_cfg: &crate::config::GasConfig, cfg: &Config, n_hops: usize) -> f64 {
        let hops = n_hops.max(1) as f64;
        if gas_cfg.estimated_gas_units > 0 {
            let overhead = cfg
                .flashloan
                .gas_overhead
                .unwrap_or(100_000)
                .min(gas_cfg.estimated_gas_units);
            let base_per_hop = (gas_cfg.estimated_gas_units - overhead) as f64 / 3.0;
            return base_per_hop * hops + overhead as f64;
        }
        if gas_cfg.default_gas_limit == 0 {
            gas_cfg.max_gas_limit as f64
        } else {
            gas_cfg.default_gas_limit as f64
        }
    }

    /// Aprende do receipt sem deixar uma tx atípica distorcer a estimativa.
    pub async fn observe_gas_used(&self, n_hops: usize, actual_units: U256) {
        let actual = actual_units.as_u64() as f64;
        if actual <= 0.0 { return; }
        let cfg = self.config.lock().await.clone();
        let estimated = Self::gas_units_for_hops(&cfg.gas, &cfg, n_hops);
        if estimated <= 0.0 { return; }
        let mut multiplier = self.gas_unit_multiplier.write().await;
        // EWMA 20% observação / 80% histórico, limitado para segurança.
        *multiplier = next_gas_multiplier(*multiplier, actual, estimated);
        crate::infra::metrics::record_gas_calibration(estimated, actual);
        info!("⛽ [GasCalibration] hops={} estimated={:.0} actual={:.0} multiplier={:.3}", n_hops, estimated, actual, *multiplier);
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

fn next_gas_multiplier(previous: f64, actual_units: f64, estimated_units: f64) -> f64 {
    if !actual_units.is_finite() || !estimated_units.is_finite() || actual_units <= 0.0 || estimated_units <= 0.0 {
        return previous.clamp(0.8, 1.5);
    }
    let ratio = (actual_units / estimated_units).clamp(0.5, 2.0);
    (previous * 0.8 + ratio * 0.2).clamp(0.8, 1.5)
}

/// Gwei fracionário → wei, sem truncar a parte decimal.
///
/// `gwei(x as u64)` descartava a fração: um oracle devolvendo 30.7 gwei virava
/// 30, e uma priority de 0.6 gwei virava **0** (tx que nunca entra em bloco).
fn gwei_f64(n: f64) -> U256 {
    if !n.is_finite() || n <= 0.0 {
        return U256::zero();
    }
    let wei = (n * 1e9).round();
    if wei >= u128::MAX as f64 {
        return U256::MAX;
    }
    U256::from(wei as u128)
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
            gas_unit_multiplier: self.gas_unit_multiplier.clone(),
            http: self.http.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gwei_f64_keeps_fraction() {
        // `gwei(x as u64)` truncava: 30.7 -> 30 gwei.
        assert_eq!(gwei_f64(30.7), U256::from(30_700_000_000u64));
        assert_eq!(gwei_f64(1.0), gwei(1));
        // Sub-gwei não pode virar zero — priority 0 nunca entra em bloco.
        assert_eq!(gwei_f64(0.6), U256::from(600_000_000u64));
        assert!(!gwei_f64(0.6).is_zero());
    }

    #[test]
    fn gwei_f64_rejects_nonfinite_and_negative() {
        assert!(gwei_f64(f64::NAN).is_zero());
        assert!(gwei_f64(-5.0).is_zero());
        assert!(gwei_f64(0.0).is_zero());
    }

    #[test]
    fn cost_uses_estimated_units_not_limit() {
        // Precificar com o TETO da tx infla o custo. Config traz consumo esperado.
        let cfg = crate::config::GasConfig::default();
        assert!(cfg.estimated_gas_units > 0);
        assert!(
            cfg.estimated_gas_units < cfg.max_gas_limit,
            "consumo esperado deve ser menor que o teto"
        );
    }

    /// M4: gas_units escala por hops reais, não fixo. Rota de 3 hops = referência
    /// (estimated_gas_units). 4 hops > 3 hops > 2 hops.
    #[test]
    fn gas_units_scale_with_hops() {
        let gas_cfg = crate::config::GasConfig::default();
        let cfg = crate::config::Config::default();
        let u3 = GasEstimator::<ethers::providers::Provider<ethers::providers::Http>>::gas_units_for_hops(&gas_cfg, &cfg, 3);
        let u2 = GasEstimator::<ethers::providers::Provider<ethers::providers::Http>>::gas_units_for_hops(&gas_cfg, &cfg, 2);
        let u4 = GasEstimator::<ethers::providers::Provider<ethers::providers::Http>>::gas_units_for_hops(&gas_cfg, &cfg, 4);
        // 3 hops deve reproduzir o estimated_gas_units de referência.
        assert!((u3 - gas_cfg.estimated_gas_units as f64).abs() < 1e-6, "3 hops={u3} ref={}", gas_cfg.estimated_gas_units);
        assert!(u4 > u3, "4 hops ({u4}) deve custar mais que 3 ({u3})");
        assert!(u2 < u3, "2 hops ({u2}) deve custar menos que 3 ({u3})");
        // 0 hops (degen) não divide por zero — clamp em 1.
        let u0 = GasEstimator::<ethers::providers::Provider<ethers::providers::Http>>::gas_units_for_hops(&gas_cfg, &cfg, 0);
        assert!(u0.is_finite() && u0 > 0.0);
    }

    #[test]
    fn calibration_ewma_tracks_receipt_without_outlier_jump() {
        assert!((next_gas_multiplier(1.0, 120.0, 100.0) - 1.04).abs() < 1e-9);
        // 10x receipt é clampado, não eleva multiplicador direto para 10x.
        assert!(next_gas_multiplier(1.0, 1_000.0, 100.0) <= 1.200_001);
    }
}
