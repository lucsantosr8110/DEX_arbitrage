// ================================================================
// src/dex/get_token_decimals.rs — v3.9.5 (CORRIGIDO + INTEGRADO)
// ================================================================
//
// ✅ CORREÇÃO: Removida lógica de metadata (get_decimals_from_metadata)
// ✅ CORREÇÃO: Rate Limiter movido para DENTRO da consulta on-chain.
// ✅ NOVO: Integração com TokenCache global para evitar chamadas on-chain
// ================================================================

// ⬇️ CORREÇÃO: Importar o rate limiter ⬇️
use crate::{
    config::token_cache::TokenCache, dex::rate_limiter::ALCHEMY_RATE_LIMITER, AppMiddleware,
};
use anyhow::Result;
use ethers::prelude::*;
use once_cell::sync::Lazy;
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::time::timeout;
use tracing::{debug, warn};

// ---------- ABI ERC20 ----------
abigen!(
    IERC20,
    "abi/ERC20.json",
    event_derives(serde::Deserialize, serde::Serialize)
);

// ================================================================
// 🗃️ Cache local process-wide (Address → u8)
// ================================================================
static DECIMALS_CACHE: Lazy<RwLock<HashMap<Address, u8>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

// ================================================================
// 🔧 Funções auxiliares de cache
// ================================================================
pub fn cache_token_decimals(token: Address, decimals: u8) {
    let mut lock = DECIMALS_CACHE.write().expect("DECIMALS_CACHE poisoned");
    lock.insert(token, decimals);
    debug!("💾 [DECIMALS CACHE SET] {token:?} => {decimals}");
}

pub fn cached_token_decimals(token: Address) -> Option<u8> {
    let lock = DECIMALS_CACHE.read().expect("DECIMALS_CACHE poisoned");
    lock.get(&token).cloned()
}

// ================================================================
// 🧩 Função principal (MODIFICADA)
// ================================================================
pub async fn get_token_decimals(client: Arc<AppMiddleware>, token_address: Address) -> Result<u8> {
    // 1️⃣ Cache local (process-wide)
    if let Some(d) = cached_token_decimals(token_address) {
        debug!("💾 [DECIMALS CACHE HIT] {token_address:?} => {d}");
        return Ok(d);
    }

    // 2️⃣ TokenCache global (NOVO - CONSULTA CONFIG)
    if let Some(token_cache) = TokenCache::global_instance() {
        if let Some(token_info) = token_cache.get_by_address(&token_address).await {
            let decimals = token_info.decimals;
            cache_token_decimals(token_address, decimals); // Popula cache interno
            debug!("🔗 [TOKEN CACHE HIT] {token_address:?} => {decimals} (via config)");
            return Ok(decimals);
        }
    }

    // 3️⃣ Consulta on-chain (Fallback - apenas se cache falhar)
    debug!("📞 [DECIMALS ON-CHAIN] {token_address:?}");
    let contract = IERC20::new(token_address, client);

    // ⬇️ CORREÇÃO: Rate limiter é chamado APENAS aqui, antes da chamada de rede ⬇️
    ALCHEMY_RATE_LIMITER.acquire().await?;

    match timeout(Duration::from_secs(2), contract.decimals().call()).await {
        Ok(Ok(decimals)) => {
            cache_token_decimals(token_address, decimals);
            Ok(decimals)
        }
        Ok(Err(e)) => {
            warn!(
                "⚠️ [DECIMALS] Erro em decimals() para {:?}: {} — SEM fallback 18",
                token_address, e
            );
            Err(anyhow::anyhow!(
                "decimals() falhou para {:?}: {} (sem fallback 18)",
                token_address,
                e
            ))
        }
        Err(_) => {
            warn!(
                "⏰ [DECIMALS] Timeout decimals() para {:?} — SEM fallback 18",
                token_address
            );
            Err(anyhow::anyhow!(
                "decimals() timeout para {:?} (sem fallback 18)",
                token_address
            ))
        }
    }
}

// ================================================================
// 📖 [REMOVIDO] Funções de metadata (fonte do bug dos decimais)
// ================================================================
// async fn find_symbol_by_address(...)
// async fn get_decimals_from_metadata(...)

// ================================================================
// 🧪 TESTES
// ================================================================
#[cfg(test)]
mod tests {
    use super::*;
    use ethers::types::H160;

    #[tokio::test]
    async fn test_cache_round_trip() {
        let addr: Address = H160::from_low_u64_be(42);
        assert!(cached_token_decimals(addr).is_none());
        cache_token_decimals(addr, 8);
        assert_eq!(cached_token_decimals(addr), Some(8));
    }
}
