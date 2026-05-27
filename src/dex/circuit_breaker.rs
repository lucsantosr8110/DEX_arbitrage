// ============================================================
// src/dex/circuit_breaker.rs — v3.8 (Compatível com Radar v3.7)
// ============================================================

use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

// ============================================================
// 🧩 Estrutura de entrada individual por DEX
// ============================================================

#[derive(Debug, Clone)]
struct DexEntry {
    failures: u32,
    last_failure: Option<Instant>,
    cooldown_until: Option<Instant>,
}

// ============================================================
// ⚙️ Estrutura principal do Circuit Breaker
// ============================================================

#[derive(Clone)]
pub struct DexCircuitBreaker {
    state: Arc<RwLock<HashMap<String, DexEntry>>>,
    max_failures: u32,
    cooldown: Duration,
}

impl DexCircuitBreaker {
    /// Cria um novo Circuit Breaker para todas as DEXs.
    pub fn new(max_failures: u32, cooldown_secs: u64) -> Self {
        Self {
            state: Arc::new(RwLock::new(HashMap::new())),
            max_failures,
            cooldown: Duration::from_secs(cooldown_secs),
        }
    }

    // ============================================================
    // 🚫 should_skip() — verifica se a DEX está em cooldown
    // ============================================================
    pub fn name(&self) -> &'static str {
        "DexCircuitBreaker"
    }

    pub fn is_empty(&self) -> bool {
        futures::executor::block_on(async { self.state.read().await.is_empty() })
    }

    /// Retorna true se a DEX estiver em cooldown (deve ser ignorada)
    pub fn should_skip(&self, dex_name: &str) -> bool {
        futures::executor::block_on(async move {
            let map = self.state.read().await;
            if let Some(entry) = map.get(dex_name) {
                if let Some(until) = entry.cooldown_until {
                    if Instant::now() < until {
                        debug!("⏸️ [{}] em cooldown até {:?}", dex_name, until);
                        return true;
                    }
                }
            }
            false
        })
    }

    // ============================================================
    // ⚠️ record_failure() — registra falhas e ativa cooldown
    // ============================================================
    pub async fn record_failure(&self, dex_name: &str) {
        let mut map = self.state.write().await;
        let entry = map.entry(dex_name.to_string()).or_insert(DexEntry {
            failures: 0,
            last_failure: None,
            cooldown_until: None,
        });

        entry.failures += 1;
        entry.last_failure = Some(Instant::now());

        if entry.failures >= self.max_failures {
            let cooldown_until = Instant::now() + self.cooldown;
            entry.cooldown_until = Some(cooldown_until);
            warn!(
                "⚠️ [{}] atingiu {} falhas — em cooldown por {:?}",
                dex_name, entry.failures, self.cooldown
            );
        } else {
            debug!(
                "⚠️ [{}] falha registrada ({}/{})",
                dex_name, entry.failures, self.max_failures
            );
        }
    }

    // ============================================================
    // ✅ record_success() — reseta contador de falhas
    // ============================================================
    pub async fn record_success(&self, dex_name: &str) {
        let mut map = self.state.write().await;
        if let Some(entry) = map.get_mut(dex_name) {
            if entry.failures > 0 {
                entry.failures = 0;
                entry.cooldown_until = None;
                debug!("✅ [{}] resetado após sucesso", dex_name);
            }
        }
    }

    // ============================================================
    // 🩺 Status utilitário
    // ============================================================
    pub async fn is_in_cooldown(&self, dex_name: &str) -> bool {
        let map = self.state.read().await;
        if let Some(entry) = map.get(dex_name) {
            if let Some(until) = entry.cooldown_until {
                return Instant::now() < until;
            }
        }
        false
    }

    pub async fn get_failure_count(&self, dex_name: &str) -> u32 {
        let map = self.state.read().await;
        map.get(dex_name).map(|e| e.failures).unwrap_or(0)
    }

    pub async fn clear(&self) {
        let mut map = self.state.write().await;
        map.clear();
        info!("🧹 Circuit breaker limpo para todas as DEXs");
    }

    pub async fn list_states(&self) -> Vec<(String, u32, bool)> {
        let map = self.state.read().await;
        map.iter()
            .map(|(dex, e)| {
                let cooldown_active = e
                    .cooldown_until
                    .map(|t| Instant::now() < t)
                    .unwrap_or(false);
                (dex.clone(), e.failures, cooldown_active)
            })
            .collect()
    }
}

// ============================================================
// 🧪 Testes
// ============================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_breaker_flow() {
        let cb = DexCircuitBreaker::new(2, 1); // 2 falhas → cooldown de 1s
        let dex = "Uniswap";

        assert!(!cb.should_skip(dex));

        cb.record_failure(dex).await;
        assert_eq!(cb.get_failure_count(dex).await, 1);
        assert!(!cb.should_skip(dex));

        cb.record_failure(dex).await;
        assert!(cb.should_skip(dex));

        tokio::time::sleep(Duration::from_secs(2)).await;
        assert!(!cb.should_skip(dex));

        cb.record_success(dex).await;
        assert_eq!(cb.get_failure_count(dex).await, 0);
    }
}
