// ============================================================
// src/dex/rate_limiter.rs — v4.2.1 (CORRIGIDO PARA E0277)
// ============================================================
//
// ✅ Corrigido: comparações Instant > Duration
// ✅ Compatível com Rust 1.77+
// ✅ Backoff adaptativo e limpeza automática
// ✅ CORREÇÃO E0277: Alterada assinatura de acquire() para
//    retornar anyhow::Result<()> (que é Send + Sync)
// ============================================================

// ⬇️ CORREÇÃO: Importar o Result do anyhow ⬇️
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct UltraRateLimiter {
    requests: Arc<Mutex<Vec<Instant>>>,
    max_requests: usize,
    time_window: Duration,
    name: String,
    start_time: Instant,
}

impl UltraRateLimiter {
    pub fn new(max_requests: usize, time_window: Duration, name: &str) -> Self {
        info!(
            "🚀 UltraRateLimiter '{}' criado: {}/{:?}",
            name, max_requests, time_window
        );
        Self {
            requests: Arc::new(Mutex::new(Vec::new())),
            max_requests,
            time_window,
            name: name.to_string(),
            start_time: Instant::now(),
        }
    }

    // ============================================================
    // 🔐 Controle de aquisição — com backoff adaptativo
    // ============================================================
    
    // ⬇️ CORREÇÃO E0277: O tipo de erro foi alterado de 
    // `Box<dyn std::error::Error>` para `anyhow::Error` (via `use anyhow::Result`)
    // para satisfazer os requisitos de Send + Sync do Tokio e do anyhow.
    pub async fn acquire(&self) -> Result<()> {
        let start = Instant::now();
        let mut retry_count = 0_u32;
        const MAX_RETRIES: u32 = 3;

        loop {
            let now = Instant::now();
            let mut requests = self.requests.lock().await;

            let elapsed = now.duration_since(self.start_time);
            let cutoff = now - std::cmp::min(elapsed, self.time_window);

            requests.retain(|&time| time >= cutoff);

            if requests.len() < self.max_requests {
                requests.push(now);
                return Ok(());
            }

            drop(requests);
            retry_count += 1;

            if retry_count > MAX_RETRIES {
                if start.elapsed() > Duration::from_secs(2) {
                    warn!(
                        "⚡ UltraRateLimiter '{}': forçando limpeza após {:?}",
                        self.name,
                        start.elapsed()
                    );

                    let mut requests = self.requests.lock().await;
                    let elapsed = now.duration_since(self.start_time);
                    let cutoff = now - std::cmp::min(elapsed, self.time_window);

                    requests.retain(|&time| time >= cutoff);

                    if requests.len() < self.max_requests {
                        requests.push(now);
                        return Ok(());
                    }

                    if !requests.is_empty() {
                        requests.remove(0);
                        requests.push(now);
                        return Ok(());
                    }
                }
            }

            let sleep_time = Duration::from_millis((25 * retry_count as u64).min(200));
            tokio::time::sleep(sleep_time).await;
        }
    }

    // ============================================================
    // 🧹 Limpeza periódica
    // ============================================================
    pub async fn cleanup(&self) {
        let mut requests = self.requests.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(self.start_time);
        let cutoff = now - std::cmp::min(elapsed, self.time_window);
        let before = requests.len();

        requests.retain(|&time| time >= cutoff);
        if before > requests.len() {
            debug!(
                "🧹 UltraRateLimiter '{}': limpou {} requests",
                self.name,
                before - requests.len()
            );
        }
    }

    pub async fn get_usage(&self) -> (usize, usize) {
        let requests = self.requests.lock().await;
        let now = Instant::now();
        let elapsed = now.duration_since(self.start_time);
        let cutoff = now - std::cmp::min(elapsed, self.time_window);
        let active_requests = requests.iter().filter(|&&time| time >= cutoff).count();
        (active_requests, self.max_requests)
    }
}

// ============================================================
// ⚠️ CORREÇÃO CRÍTICA (Erro 429) ⚠️
// Os valores 500/s e 200/s estavam muito altos para o
// limite de CUPS (Compute Units) da Alchemy.
// Reduzidos para valores conservadores (20/s e 40/s).
// ============================================================
lazy_static::lazy_static! {
    pub static ref ALCHEMY_RATE_LIMITER: UltraRateLimiter =
        UltraRateLimiter::new(20, Duration::from_secs(1), "alchemy_main");

    pub static ref DEX_RATE_LIMITER: UltraRateLimiter =
        UltraRateLimiter::new(40, Duration::from_secs(1), "dex_calls");
}