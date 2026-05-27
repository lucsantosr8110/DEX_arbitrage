// ================================================================
// src/config/token_cache.rs — v3.9.23-stable-fix-decimals (CORRIGIDO)
// ================================================================
//
// ✅ Corrigido conflito Option<u8> vs u8
// ✅ Usa .and_then(|meta| meta.decimals).unwrap_or(18)
// ✅ Usa decimals.unwrap_or(18) ao construir TokenInfo
// ✅ Corrigido debug! para {:?}
// ✅ CORREÇÃO: Removido delimitador } inesperado (Fixa erro de compilação E0106)
// ✅ CORREÇÃO: Consolidada e restaurada a lógica das funções get_by_address e get_by_symbol
// ================================================================

use ethers::types::Address;
use prometheus::{register_int_gauge, IntGauge};
use std::{collections::HashMap, str::FromStr, sync::Arc};
use tokio::sync::{OnceCell, RwLock};
use tracing::{debug, info, warn};

use crate::config::Config;

// ================================================================
// 🧠 Estruturas de dados
// ================================================================

#[derive(Debug, Clone)]
pub struct TokenInfo {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
}

#[derive(Clone)]
pub struct TokenCache {
    inner: Arc<RwLock<HashMap<String, TokenInfo>>>, // 🔄 String -> TokenInfo
    metrics: TokenCacheMetrics,
}

// ================================================================
// 📊 Métricas Prometheus
// ================================================================

#[derive(Clone)]
struct TokenCacheMetrics {
    size: IntGauge,
}

impl TokenCacheMetrics {
    fn new() -> Self {
        Self {
            size: register_int_gauge!("token_cache_size", "Número atual de tokens no cache global")
                .unwrap_or_else(|_| IntGauge::new("token_cache_size_fallback", "size").unwrap()),
        }
    }
}

// ================================================================
// 🧩 Implementação principal
// ================================================================

impl TokenCache {
    /// Inicializa o cache com os endereços e decimals definidos no config.toml
    pub async fn new_from_config(config: Arc<Config>) -> Self {
        let mut map = HashMap::new();
        let metrics = TokenCacheMetrics::new();

        let pairs_cfg = &config.pairs;
        let tokens_map = &pairs_cfg.tokens;
        let metadata_map = &pairs_cfg.metadata;

        for (symbol, addr_str) in tokens_map {
            match Address::from_str(addr_str) {
                Ok(address) => {
                    // Busca decimals do metadata ou usa fallback
                    let decimals = metadata_map
                        .get(symbol)
                        .and_then(|meta| meta.decimals)
                        .unwrap_or(18);

                    let token_info = TokenInfo {
                        address,
                        symbol: symbol.clone(),
                        decimals,
                    };

                    map.insert(symbol.to_uppercase(), token_info);
                    debug!(
                        "🔹 TokenCache: carregado {} -> {} (decimals: {})",
                        symbol, address, decimals
                    );
                }
                Err(e) => warn!("⚠️ TokenCache: endereço inválido para {}: {}", symbol, e),
            }
        }

        if map.is_empty() {
            warn!("⚠️ TokenCache: seção [pairs.tokens] vazia no config.toml");
        }

        metrics.size.set(map.len() as i64);
        info!("✅ TokenCache inicializado com {} tokens (via config.toml)", map.len());

        Self {
            inner: Arc::new(RwLock::new(map)),
            metrics,
        }
    }

    /// Resolve endereço de token pelo símbolo (ex: "USDC") ou literal 0x...
    pub async fn resolve(&self, token: &str) -> Option<Address> {
        let t = token.trim();
        if t.starts_with("0x") && t.len() == 42 {
            if let Ok(addr) = Address::from_str(t) {
                debug!("🔍 TokenCache: endereço literal reconhecido [{}]", t);
                return Some(addr);
            }
        }

        let map = self.inner.read().await;
        if let Some(token_info) = map.get(&t.to_uppercase()) {
            debug!("🔍 TokenCache HIT [{}]", t);
            Some(token_info.address)
        } else {
            warn!("⚠️ TokenCache MISS [{}]", t);
            None
        }
    }

    /// 🔥 Busca TokenInfo por endereço
    pub async fn get_by_address(&self, address: &Address) -> Option<TokenInfo> {
        let map = self.inner.read().await;
        map.values()
            .find(|token_info| &token_info.address == address)
            .cloned()
    }

    /// 🔥 Busca TokenInfo por símbolo
    pub async fn get_by_symbol(&self, symbol: &str) -> Option<TokenInfo> {
        let map = self.inner.read().await;
        map.get(&symbol.to_uppercase()).cloned()
    }

    /// 🔥 Busca decimals por endereço
    pub async fn get_decimals_by_address(&self, address: &Address) -> Option<u8> {
        self.get_by_address(address).await.map(|info| info.decimals)
    }

    /// Adiciona novo token dinamicamente
    pub async fn insert(&self, symbol: &str, address: Address, decimals: u8) {
        let mut map = self.inner.write().await;
        let token_info = TokenInfo {
            address,
            symbol: symbol.to_string(),
            decimals,
        };
        map.insert(symbol.to_uppercase(), token_info);
        self.metrics.size.set(map.len() as i64);
        info!(
            "➕ TokenCache: adicionado {} -> {} (decimals: {})",
            symbol, address, decimals
        );
    }

    /// Recarrega tokens a partir de um novo config (hot reload)
    pub async fn reload_from_config(&self, config: Arc<Config>) {
        let mut new_map = HashMap::new();
        let pairs_cfg = &config.pairs;
        let tokens_map = &pairs_cfg.tokens;
        let metadata_map = &pairs_cfg.metadata;

        for (symbol, addr_str) in tokens_map {
            if let Ok(address) = Address::from_str(addr_str) {
                let decimals = metadata_map
                    .get(symbol)
                    .and_then(|meta| meta.decimals)
                    .unwrap_or(18);

                let token_info = TokenInfo {
                    address,
                    symbol: symbol.clone(),
                    decimals,
                };
                new_map.insert(symbol.to_uppercase(), token_info);
            }
        }

        let mut map = self.inner.write().await;
        *map = new_map;
        self.metrics.size.set(map.len() as i64);
        info!(
            "🔄 TokenCache recarregado com {} tokens (via config)",
            map.len()
        );
    }

    /// Remove um token
    pub async fn remove(&self, symbol: &str) -> bool {
        let mut map = self.inner.write().await;
        let removed = map.remove(&symbol.to_uppercase()).is_some();
        self.metrics.size.set(map.len() as i64);
        removed
    }

    /// Lista todos os tokens
    pub async fn list(&self) -> Vec<TokenInfo> {
        self.inner.read().await.values().cloned().collect()
    }

    /// Retorna o tamanho do cache
    pub async fn size(&self) -> usize {
        self.inner.read().await.len()
    }

    /// Acesso global via OnceCell
    pub async fn global(config: Arc<Config>) -> Arc<Self> {
        GLOBAL_TOKEN_CACHE
            .get_or_init(|| async { Arc::new(Self::new_from_config(config.clone()).await) })
            .await
            .clone()
    }

    /// 🔥 Acesso à instância global sem config (leitura)
    pub fn global_instance() -> Option<Arc<Self>> {
        GLOBAL_TOKEN_CACHE.get().cloned()
    }
}

// ================================================================
// 🌍 Instância global única
// ================================================================
pub static GLOBAL_TOKEN_CACHE: OnceCell<Arc<TokenCache>> = OnceCell::const_new();

// ================================================================
// 🧪 TESTES - COM CORREÇÃO DO DEFAULT CONFIG
// ================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::Address;
    use std::collections::HashMap;

    // ⚠️ ATENÇÃO: Se as structs no src/config/mod.rs não tiverem #[derive(Default)]
    // ou impl Default para TODOS os campos, a linha abaixo falhará em tempo de execução
    // ou compilação se a trait Default não for implementada para Config.
    fn create_test_config() -> Config {
        use crate::config::{PairsConfig, TokenMetadata};

        let mut tokens = HashMap::new();
        tokens.insert("TEST".to_string(), "0x000000000000000000000000000000000000abcd".to_string());

        let mut metadata = HashMap::new();
        metadata.insert(
            "TEST".to_string(),
            TokenMetadata {
                symbol: "TEST".to_string(),
                name: Some("TEST Token".into()),
                decimals: Some(8),
                coingecko_id: None,
                category: None,
            },
        );

        Config {
            pairs: PairsConfig {
                tokens,
                metadata,
                ..Default::default()
            },
            min_profit_usd_threshold: 0.000005,
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn test_token_resolution() {
        let dummy_cfg = create_test_config(); // 🔧 Use a função de criação

        let cache = TokenCache::new_from_config(Arc::new(dummy_cfg)).await;

        let addr = cache.resolve("TEST").await.unwrap();
        assert_eq!(
            addr,
            Address::from_str("0x000000000000000000000000000000000000abcd").unwrap()
        );

        let token_info = cache.get_by_address(&addr).await.unwrap();
        assert_eq!(token_info.decimals, 8);
        assert_eq!(token_info.symbol, "TEST");
    }

    #[tokio::test]
    async fn test_token_cache_operations() {
        let dummy_cfg = create_test_config(); // 🔧 Use a função de criação

        let cache = TokenCache::new_from_config(Arc::new(dummy_cfg)).await;

        // Teste de inserção dinâmica
        cache.insert(
            "NEWTOKEN",
            Address::from_str("0x0000000000000000000000000000000000001234").unwrap(),
            6,
        )
        .await;

        // Teste de resolução
        let addr = cache.resolve("NEWTOKEN").await.unwrap();
        assert_eq!(
            addr,
            Address::from_str("0x0000000000000000000000000000000000001234").unwrap()
        );

        // Teste de get_by_symbol
        let token_info = cache.get_by_symbol("NEWTOKEN").await.unwrap();
        assert_eq!(token_info.decimals, 6);
        assert_eq!(token_info.symbol, "NEWTOKEN");

        // Teste de listagem
        let tokens = cache.list().await;
        assert!(tokens.len() >= 2); // Pelo menos TEST + NEWTOKEN

        // Teste de remoção
        assert!(cache.remove("NEWTOKEN").await);
        assert!(cache.resolve("NEWTOKEN").await.is_none());
    }

    #[tokio::test]
    async fn test_token_cache_size() {
        let dummy_cfg = create_test_config(); // 🔧 Use a função de criação

        let cache = TokenCache::new_from_config(Arc::new(dummy_cfg)).await;
        
        let initial_size = cache.size().await;
        assert!(initial_size >= 1); // Pelo menos o token TEST

        // Adiciona um token e verifica o tamanho
        cache.insert(
            "SIZE_TEST",
            Address::from_str("0x0000000000000000000000000000000000009999").unwrap(),
            18,
        )
        .await;

        let new_size = cache.size().await;
        assert_eq!(new_size, initial_size + 1);
    }

    #[tokio::test]
    async fn test_address_literal_resolution() {
        let dummy_cfg = create_test_config(); // 🔧 Use a função de criação

        let cache = TokenCache::new_from_config(Arc::new(dummy_cfg)).await;

        // Teste com endereço literal
        let literal_addr = "0x000000000000000000000000000000000000abcd";
        let resolved = cache.resolve(literal_addr).await.unwrap();
        assert_eq!(
            resolved,
            Address::from_str("0x000000000000000000000000000000000000abcd").unwrap()
        );
    }
}
