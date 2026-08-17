// ================================================================
// src/core/smart_retry.rs — v3.9.2 (Seguro e Send-compatível)
// ================================================================
//
// ✅ Usa StdRng::from_entropy() para compatibilidade com tokio::spawn
// ✅ Backoff exponencial com jitter assimétrico
// ✅ Compatível com radar.rs v3.9 e DexManager v3.9.1
// ✅ [CORRIGIDO] Erro de sintaxe std.future -> std::future
// ✅ [CORRIGIDO] Removidos 10 warnings de unused imports
// ================================================================

use anyhow::Result;
use rand::{rngs::StdRng, Rng, SeedableRng};
use std::{
    fmt::Debug,
    time::{Duration, Instant},
};
use tokio::time::sleep;
use tracing::{debug, info, warn};

#[derive(Clone)]
pub struct SmartRetryManager {
    pub max_retries: usize,
    pub base_delay: Duration,
    pub jitter: f64,
    pub priority_mode: bool, // 🔹 Se ativo, reduz drasticamente o delay (para health/radar)
}

impl SmartRetryManager {
    // ============================================================
    // 🔧 Inicialização
    // ============================================================
    pub fn new(max_retries: usize, base_delay: Duration) -> Self {
        Self {
            max_retries,
            base_delay,
            jitter: 0.15,
            priority_mode: false,
        }
    }

    pub fn with_jitter(mut self, jitter: f64) -> Self {
        self.jitter = jitter.clamp(0.0, 1.0);
        self
    }

    pub fn with_priority(mut self, enabled: bool) -> Self {
        self.priority_mode = enabled;
        self
    }

    // ============================================================
    // 🔁 Execução resiliente com backoff exponencial + jitter
    // ============================================================
    pub async fn exec<F, Fut, T, E>(&self, ctx: &str, mut operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        // ✅ CORREÇÃO E0308: std.future -> std::future
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: Debug + Send + Sync + 'static,
    {
        let start_time = Instant::now();
        let mut delay = self.base_delay;
        let mut rng = StdRng::from_entropy(); // ✅ compatível com tokio::spawn

        for attempt in 0..=self.max_retries {
            match operation().await {
                Ok(v) => {
                    debug!(
                        "✅ [{}] sucesso na tentativa {} (elapsed: {:?})",
                        ctx,
                        attempt + 1,
                        start_time.elapsed()
                    );
                    return Ok(v);
                }
                Err(e) if attempt < self.max_retries => {
                    // cálculo do delay com jitter assimétrico
                    let mut delay_ms = delay.as_millis() as f64;
                    if self.jitter > 0.0 {
                        let factor = 1.0 + rng.gen_range(0.0..(2.0 * self.jitter));
                        delay_ms *= factor;
                    }

                    // prioridade = retries mais curtos
                    if self.priority_mode {
                        delay_ms *= 0.4;
                    }

                    warn!(
                        "🔁 [{}] tentativa {}/{} falhou: {:?}. Retentando em {:.0}ms",
                        ctx,
                        attempt + 1,
                        self.max_retries,
                        e,
                        delay_ms
                    );

                    sleep(Duration::from_millis(delay_ms as u64)).await;
                    delay *= 2;
                }
                Err(e) => {
                    warn!(
                        "❌ [{}] falhou após {} tentativas: {:?} (elapsed: {:?})",
                        ctx,
                        attempt + 1,
                        e,
                        start_time.elapsed()
                    );
                    return Err(anyhow::anyhow!("{:?}", e));
                }
            }
        }

        unreachable!("Loop de retry deve terminar antes do unreachable!()")
    }

    // ============================================================
    // 🔄 Alias compatível com radar.rs v3.9 (mais semântico)
    // ============================================================
    pub async fn execute_with_retry<F, Fut, T, E>(&self, operation: F) -> Result<T>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: Debug + Send + Sync + 'static,
    {
        self.exec("SmartRetry", operation).await
    }

    // ============================================================
    // 🧪 Teste interno de performance (diagnóstico)
    // ============================================================
    pub async fn benchmark(&self) {
        info!(
            "🧩 SmartRetry benchmark: {} retries, base_delay={:?}, jitter={:.2}",
            self.max_retries, self.base_delay, self.jitter
        );

        let start = Instant::now();
        let _ = self
            .exec("benchmark", || async { Err::<(), _>("simulated_failure") })
            .await
            .err();

        info!("⏱️ Benchmark concluído em {:?}", start.elapsed());
    }
}
