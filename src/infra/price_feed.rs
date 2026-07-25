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
use tracing::debug;

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

/// Instância global do feed.
///
/// O `CACHE` acima já é estático, então múltiplas instâncias compartilhariam os
/// preços de qualquer forma — mas cada `CachedPriceFeed::new()` constrói um
/// `reqwest::Client` novo, com seu próprio pool de conexões. Uma instância só
/// evita esse desperdício nos adapters, que precisam do feed a cada ciclo.
pub static PRICE_FEED: Lazy<CachedPriceFeed> = Lazy::new(CachedPriceFeed::new);

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
                // Cache NEGATIVO: sem isto, cada cotação re-bate na Coingecko, que
                // rate-limita a API gratuita e gera milhares de WARN por minuto.
                // O fallback heurístico é suficiente para DIMENSIONAR o notional
                // (o preço que vale para lucro é sempre o que o DEX devolve), então
                // cacheamos o fallback pelo mesmo TTL e seguimos em silêncio.
                let price = Self::fallback_price(symbol);
                {
                    let mut cache = CACHE.write().unwrap();
                    cache.insert(
                        symbol.to_lowercase(),
                        CachedEntry {
                            price_usd: price,
                            timestamp: Instant::now(),
                        },
                    );
                }
                debug!(target: "price_feed", symbol, error = %e, fallback = price, "preço via fallback heurístico (cacheado)");
                Ok(price)
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
    pub(crate) fn map_symbol(symbol: &str) -> String {
        match symbol.to_uppercase().as_str() {
            "USDC" => "usd-coin".to_string(),
            "USDT" => "tether".to_string(),
            "DAI" => "dai".to_string(),
            "WETH" | "ETH" => "ethereum".to_string(),
            "WBTC" | "BTC" => "bitcoin".to_string(),
            // Token de gás da Polygon PoS = POL (`0x...1010`), ex-MATIC.
            //
            // NÃO usar `matic-network`: é a página legada do MATIC, congelada em
            // 2025-10-17 com market cap 0 e volume ~$0.15. Ela ainda responde à
            // API e devolvia ~$0.126 contra ~$0.077 do POL real — 64% de
            // superestimativa em cima de TODO custo de gás do bot.
            "WMATIC" | "MATIC" | "POL" | "WPOL" => "polygon-ecosystem-token".to_string(),
            "BNB" => "binancecoin".to_string(),
            "AVAX" => "avalanche-2".to_string(),
            "LINK" => "chainlink".to_string(),
            "UNI" => "uniswap".to_string(),
            "LDO" => "lido-dao".to_string(),
            _ => symbol.to_lowercase(),
        }
    }

    /// 🔹 Fallback heurístico — só quando o Coingecko falha.
    ///
    /// Serve para DIMENSIONAR notional de cotação e precificar gás. Números
    /// aferidos em 2026-07-25; envelhecem, então erram para perto do real e não
    /// para "seguro alto" — um preço de POL inflado vira custo de gás inflado, e
    /// em micro-arbitragem isso mata rota lucrativa.
    pub fn fallback_price(symbol: &str) -> f64 {
        match symbol.to_uppercase().as_str() {
            "USDC" | "USDT" | "DAI" => 1.0,
            "WETH" | "ETH" => 1875.0,
            "WBTC" | "BTC" => 64_000.0,
            // POL ≈ $0.077 (era 0.75 aqui: 10x acima do real).
            "WMATIC" | "MATIC" | "POL" | "WPOL" => 0.077,
            "LINK" => 8.4,
            "UNI" => 3.7,
            "LDO" => 0.37,
            "BNB" => 500.0,
            _ => 1.0,
        }
    }
}
#[cfg(test)]
mod tests {
    use super::CachedPriceFeed;

    /// O token de gás da Polygon é POL. `matic-network` é a página legada do
    /// MATIC, congelada em 2025-10-17 (mcap 0, volume ~$0.15) — ainda responde
    /// à API devolvendo ~$0.126 contra ~$0.077 do POL real, inflando em 64% todo
    /// custo de gás calculado pelo bot.
    #[test]
    fn gas_token_maps_to_pol_not_dead_matic_feed() {
        for sym in ["WMATIC", "MATIC", "POL", "WPOL", "wmatic"] {
            assert_eq!(
                CachedPriceFeed::map_symbol(sym),
                "polygon-ecosystem-token",
                "{sym} deve mapear para POL"
            );
        }
    }

    #[test]
    fn fallback_gas_price_is_not_10x_off() {
        // Era 0.75 (10x acima do POL real). Fallback inflado = gás inflado =
        // micro-arbitragem descartada por custo que não existe.
        let p = CachedPriceFeed::fallback_price("WMATIC");
        assert!(p > 0.01 && p < 0.30, "fallback POL fora de faixa: {p}");
        assert_eq!(p, CachedPriceFeed::fallback_price("POL"));
    }

    #[test]
    fn stablecoin_fallbacks_are_one() {
        for sym in ["USDC", "USDT", "DAI"] {
            assert_eq!(CachedPriceFeed::fallback_price(sym), 1.0);
        }
    }
}
