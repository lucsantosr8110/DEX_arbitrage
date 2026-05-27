// ============================================================
// src/infra/price_feed.rs — v1.0.0 (GENERIC MARKET FEED)
// ============================================================
//
// ✅ Consulta Coingecko com cache local (TTL = 2 min)
// ✅ Compatível com ArbitrageEngine v4.8.x
// ✅ Thread-safe (RwLock global)
// ✅ Fallback automático se API falhar
//
// ============================================================

use anyhow::{anyhow, Result};
use once_cell::sync::Lazy;
use reqwest::Client;
// ❌ REMOVIDO: Deserialize (não usado)
use std::{
    collections::HashMap,
    sync::RwLock,
    time::{Duration, Instant},
};
use tracing::{debug, warn};

// ============================================================
// 📊 Estrutura principal
// ============================================================

#[derive(Clone, Debug)]
struct CachedEntry {
    price_usd: f64,
    timestamp: Instant,
}

#[derive(Clone)]
pub struct CachedPriceFeed {
    client: Client,
    ttl: Duration,
}

static CACHE: Lazy<RwLock<HashMap<String, CachedEntry>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// ============================================================
// 🔹 Implementação
// ============================================================

impl CachedPriceFeed {
    /// Cria nova instância com TTL de 2 minutos
    pub fn new() -> Self {
        Self {
            client: Client::builder()
                .user_agent("FlashloanBot/1.0")
                .timeout(Duration::from_secs(8))
                .build()
                .unwrap(),
            ttl: Duration::from_secs(120),
        }
    }

    /// 🔍 Obtém preço em USD, com cache e fallback seguro
    pub async fn get_price(&self, symbol: &str) -> Result<f64> {
        // 1️⃣ Verifica cache
        {
            let cache = CACHE.read().unwrap();
            if let Some(entry) = cache.get(&symbol.to_lowercase()) {
                if entry.timestamp.elapsed() < self.ttl {
                    debug!(target: "price_feed", symbol, price_usd = entry.price_usd, "💾 Cache HIT");
                    return Ok(entry.price_usd);
                }
            }
        }

        // 2️⃣ Atualiza via Coingecko
        match self.fetch_from_coingecko(symbol).await {
            Ok(price) => {
                let mut cache = CACHE.write().unwrap();
                cache.insert(
                    symbol.to_lowercase(),
                    CachedEntry {
                        price_usd: price,
                        timestamp: Instant::now(),
                    },
                );
                debug!(target: "price_feed", symbol, price_usd = price, "🌐 Cache MISS — atualizando");
                Ok(price)
            }
            Err(e) => {
                warn!(target: "price_feed", symbol, error = %e, "⚠️ Falha ao consultar preço — fallback heurístico");
                Ok(Self::fallback_price(symbol))
            }
        }
    }

    /// 🔹 Consulta simples à API pública do Coingecko
    async fn fetch_from_coingecko(&self, symbol: &str) -> Result<f64> {
        let id = Self::map_symbol(symbol);
        let url = format!(
            "https://api.coingecko.com/api/v3/simple/price?ids={}&vs_currencies=usd",
            id
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await?
            .json::<serde_json::Value>()
            .await?;
        let price = resp
            .get(&id)
            .and_then(|v| v.get("usd"))
            .and_then(|v| v.as_f64())
            .ok_or_else(|| anyhow!("Preço não encontrado para {}", symbol))?;
        Ok(price)
    }

    /// 🔸 Traduz símbolo local → ID Coingecko
    // ✅ CORREÇÃO (E0308): Adicionado .to_string() para retornar String
    fn map_symbol(symbol: &str) -> String {
        match symbol.to_uppercase().as_str() {
            "USDC" => "usd-coin".to_string(),
            "USDT" => "tether".to_string(),
            "DAI" => "dai".to_string(),
            "WETH" | "ETH" => "ethereum".to_string(),
            "WBTC" | "BTC" => "bitcoin".to_string(),
            "WMATIC" | "MATIC" => "matic-network".to_string(),
            "BNB" => "binancecoin".to_string(),
            "AVAX" => "avalanche-2".to_string(),
            _ => symbol.to_lowercase(),
        }
    }

    /// 🔹 Fallback seguro (heurístico)
    fn fallback_price(symbol: &str) -> f64 {
        match symbol.to_uppercase().as_str() {
            "USDC" | "USDT" | "DAI" => 1.0,
            "WETH" | "ETH" => 1800.0,
            "WBTC" | "BTC" => 68000.0,
            "WMATIC" | "MATIC" => 0.75,
            "BNB" => 500.0,
            _ => 1.0,
        }
    }
}