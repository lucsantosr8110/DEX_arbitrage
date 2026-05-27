// ============================================================
// src/execution/execution_engine.rs — v4.8.4-COOLDOWN-FIXED
// ------------------------------------------------------------
// - 🟢 CORREÇÃO CRÍTICA: Cooldown totalmente atômico
// - 🟢 REMOÇÃO: Métodos deprecated que causam condições de corrida
// - 🟢 MELHORIA: API simplificada e mais segura
// ============================================================

use anyhow::{anyhow, Context, Result};
use ethers::{
    providers::Middleware,
    signers::Signer,
    types::{
        transaction::eip2718::TypedTransaction, Address, BlockId, BlockNumber, Bytes,
        Eip1559TransactionRequest, H256, U256,
    },
    utils::keccak256,
};
use std::{
    collections::HashMap,
    fmt,
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, trace, warn};

use crate::execution::bundle_sender::{BundleSender, MevConfig};

// ============================================================
// 1) Erro simples para o NonceManager
// ============================================================
#[derive(Debug)]
pub enum NonceError {
    ResyncInProgress,
}

impl fmt::Display for NonceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NonceError::ResyncInProgress => write!(f, "Resync de Nonce em progresso."),
        }
    }
}
impl std::error::Error for NonceError {}

// ============================================================
// 2) NonceManager Híbrido (lock-free + resync)
// ============================================================
pub struct NonceManager {
    local_nonce: Arc<AtomicU64>,
    is_resyncing: Arc<AtomicBool>,
    resync_lock: Arc<Mutex<()>>,
}

impl NonceManager {
    pub async fn new<M: Middleware + 'static>(
        provider: Arc<M>,
        sender: Address,
    ) -> Result<Arc<Self>> {
        let initial_nonce = provider
            .get_transaction_count(sender, Some(BlockId::Number(BlockNumber::Pending)))
            .await
            .context("Falha ao buscar nonce inicial (pending)")?
            .as_u64();

        info!("NonceManager(HYBRID) iniciado — nonce inicial = {}", initial_nonce);

        Ok(Arc::new(Self {
            local_nonce: Arc::new(AtomicU64::new(initial_nonce)),
            is_resyncing: Arc::new(AtomicBool::new(false)),
            resync_lock: Arc::new(Mutex::new(())),
        }))
    }

    pub fn get_next_nonce(&self) -> Result<u64, NonceError> {
        if self.is_resyncing.load(Ordering::SeqCst) {
            warn!("Resync em andamento — adiando execução.");
            return Err(NonceError::ResyncInProgress);
        }
        Ok(self.local_nonce.fetch_add(1, Ordering::SeqCst))
    }

    pub async fn trigger_resync<M: Middleware + 'static>(
        &self,
        provider: Arc<M>,
        sender: Address,
    ) {
        if let Ok(_guard) = self.resync_lock.try_lock() {
            self.is_resyncing.store(true, Ordering::SeqCst);
            warn!("⟲ Iniciando resync de nonce (pending)...");
            let res = provider
                .get_transaction_count(sender, Some(BlockId::Number(BlockNumber::Pending)))
                .await;

            match res {
                Ok(real_nonce_u256) => {
                    let real = real_nonce_u256.as_u64();
                    let local = self.local_nonce.load(Ordering::SeqCst);
                    if real != local {
                        warn!("⚠️ NONCE DRIFT: local={} vs rede(pending)={}", local, real);
                        self.local_nonce.store(real, Ordering::SeqCst);
                        info!("✅ Nonce realinhado para {}", real);
                    } else {
                        info!("✔️ Sem drift: nonce local {} = rede {}", local, real);
                    }
                }
                Err(e) => error!("❌ Falha ao ressincronizar nonce: {e:#}"),
            }

            self.is_resyncing.store(false, Ordering::SeqCst);
            warn!("⟲ Resync concluído.");
            return;
        }

        info!("Resync já em andamento — aguardando término...");
        let _ = self.resync_lock.lock().await;
    }
}

// ============================================================
// 3) ExecutionTracker (cooldown por par, ATÔMICO CORRIGIDO)
// ============================================================
pub struct ExecutionTracker {
    last_execution: RwLock<HashMap<String, Instant>>,
    cooldown: Duration,
}

impl ExecutionTracker {
    pub fn new(cooldown_seconds: u64) -> Arc<Self> {
        Arc::new(Self {
            last_execution: RwLock::new(HashMap::new()),
            cooldown: Duration::from_secs(cooldown_seconds),
        })
    }

    #[inline]
    pub fn normalize_pair_key(pair: &str) -> String {
        pair.trim()
            .to_ascii_uppercase()
            .replace(char::is_whitespace, "")
            .replace('→', "-")
            .replace('|', "-")
            .replace('/', "-")
            .replace('_', "-")
    }

    /// ✅ CORREÇÃO CRÍTICA: ÚNICO método para cooldown - totalmente atômico
    /// Retorna:
    /// - Ok(()) se cooldown foi iniciado com sucesso
    /// - Err(Duration) com tempo restante se ainda estiver em cooldown
    pub async fn try_start_cooldown(&self, pair: &str) -> Result<(), Duration> {
        let key = Self::normalize_pair_key(pair);
        let mut map = self.last_execution.write().await;

        if let Some(last) = map.get(&key) {
            let elapsed = last.elapsed();
            if elapsed < self.cooldown {
                let remaining = self.cooldown - elapsed;
                warn!("⏳ (atomic) '{}' ainda em cooldown — faltam {:.2}s", key, remaining.as_secs_f64());
                return Err(remaining);
            }
        }

        map.insert(key.clone(), Instant::now());
        trace!("Cooldown iniciado (atomic) para '{}'", key);
        Ok(())
    }

    /// Tempo restante em segundos (apenas leitura, para métricas)
    pub async fn remaining_secs(&self, pair: &str) -> Option<f64> {
        let key = Self::normalize_pair_key(pair);
        let map = self.last_execution.read().await;
        if let Some(last) = map.get(&key) {
            let elapsed = last.elapsed();
            if elapsed < self.cooldown {
                return Some((self.cooldown - elapsed).as_secs_f64());
            }
        }
        None
    }
}

// ============================================================
// 4) ExecutionEngine (MEV + EIP-1559 + HYBRID Nonce + COOLDOWN ATÔMICO)
// ============================================================
pub struct ExecutionEngine<M, S: Signer + 'static> {
    provider: Arc<M>,
    signer: Arc<S>,
    bundle: Arc<BundleSender<S>>,
    tracker: Arc<ExecutionTracker>,
    executor: Address,
    min_priority_fee_wei: Option<U256>,
    nonce_mgr: Arc<NonceManager>,
}

impl<M, S> ExecutionEngine<M, S>
where
    M: Middleware + 'static,
    S: Signer + 'static,
{
    pub async fn new(
        provider: Arc<M>,
        signer: Arc<S>,
        mev_cfg: MevConfig,
        cooldown_seconds: u64,
        executor_address: Address,
        min_priority_fee_wei: Option<U256>,
    ) -> Result<Arc<Self>> {
        let bundle = BundleSender::new(mev_cfg, signer.clone())?;
        let tracker = ExecutionTracker::new(cooldown_seconds);
        let nonce_mgr = NonceManager::new(provider.clone(), signer.address()).await?;

        let engine = Arc::new(Self {
            provider: provider.clone(),
            signer: signer.clone(),
            bundle,
            tracker,
            executor: executor_address,
            min_priority_fee_wei,
            nonce_mgr: nonce_mgr.clone(),
        });

        let interval = std::env::var("ENGINE_RESYNC_SECS")
            .ok()
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or(20);

        tokio::spawn(Self::periodic_resync_task(
            nonce_mgr,
            provider,
            signer.address(),
            interval,
        ));

        Ok(engine)
    }

    async fn periodic_resync_task(
        nonce_mgr: Arc<NonceManager>,
        provider: Arc<M>,
        sender: Address,
        interval_secs: u64,
    ) {
        loop {
            tokio::time::sleep(Duration::from_secs(interval_secs)).await;
            nonce_mgr.trigger_resync(provider.clone(), sender).await;
        }
    }

    /// ✅ CORREÇÃO CRÍTICA: Execução com cooldown TOTALMENTE ATÔMICO
    pub async fn execute_opportunity(
        &self,
        opportunity_calldata: Bytes,
        pair_label: &str,
        estimated_profit_usd: f64,
        gas_params: (U256, U256),
        gas_limit: Option<U256>,
        value: Option<U256>,
    ) -> Result<H256> {
        // ✅ CORREÇÃO: Cooldown TOTALMENTE ATÔMICO - única operação
        match self.tracker.try_start_cooldown(pair_label).await {
            Ok(()) => {
                debug!("✅ Cooldown iniciado para '{}'", pair_label);
            }
            Err(remaining) => {
                return Err(anyhow!(
                    "Oportunidade em cooldown para '{}' — aguarde {:.2}s",
                    ExecutionTracker::normalize_pair_key(pair_label),
                    remaining.as_secs_f64()
                ));
            }
        }

        let nonce_u64 = match self.nonce_mgr.get_next_nonce() {
            Ok(n) => n,
            Err(NonceError::ResyncInProgress) => {
                return Err(anyhow!("Execução adiada: resync de nonce em curso"));
            }
        };
        let nonce = U256::from(nonce_u64);

        let chain_id = self
            .provider
            .get_chainid()
            .await
            .context("Falha ao obter chain_id")?
            .as_u64();

        let (max_fee, mut max_prio) = gas_params;
        if let Some(floor) = self.min_priority_fee_wei {
            if max_prio < floor {
                debug!("Ajustando priority_fee: {} -> {} (floor)", max_prio, floor);
                max_prio = floor;
            }
        }

        let mut req = Eip1559TransactionRequest::new()
            .to(self.executor)
            .data(opportunity_calldata.clone())
            .nonce(nonce)
            .chain_id(chain_id)
            .max_fee_per_gas(max_fee)
            .max_priority_fee_per_gas(max_prio);

        if let Some(v) = value {
            req = req.value(v);
        }

        let gas_final = if let Some(g) = gas_limit {
            trace!("Usando gas_limit explícito: {}", g);
            g
        } else {
            debug!("Estimando gas (eth_estimateGas)...");
            let mut tx_for_estimate: TypedTransaction = req.clone().into();
            tx_for_estimate.set_from(self.signer.address());

            let est = self
                .provider
                .estimate_gas(&tx_for_estimate, None)
                .await
                .context("Falha ao estimar gás")?;
            let buf = (est * 110u32) / 100u32;
            trace!("Gas estimado={} | com buffer(10%)={}", est, buf);
            buf
        };

        let tx: TypedTransaction = req.gas(gas_final).into();

        let sig = self
            .signer
            .sign_transaction(&tx)
            .await
            .context("Falha ao assinar EIP-1559")?;
        let raw_rlp = tx.rlp_signed(&sig);
        let raw_bytes = Bytes::from(raw_rlp.to_vec());
        let local_hash = H256::from_slice(keccak256(&raw_bytes).as_slice());

        let current_block = self
            .provider
            .get_block_number()
            .await
            .context("Falha ao obter blockNumber")?;

        let pair_key = ExecutionTracker::normalize_pair_key(pair_label);
        info!(
            "→ Enviando bundle [pair='{}' profit=${:.6} nonce={} block={} tx={:?}]",
            pair_key,
            estimated_profit_usd,
            nonce_u64,
            current_block,
            local_hash
        );

        match self
            .bundle
            .send_atomic_flashloan(raw_bytes, current_block.as_u64().into())
            .await
        {
            Ok(_ack) => {
                info!("✅ Bundle MEV submetido com sucesso. tx_local={:?}", local_hash);
                Ok(local_hash)
            }
            Err(e) => {
                error!("❌ Falha no envio do bundle: {e:#}");
                self.nonce_mgr
                    .trigger_resync(self.provider.clone(), self.signer.address())
                    .await;
                Err(e)
            }
        }
    }

    pub async fn check_cooldown(&self, pair: &str) -> Option<f64> {
        self.tracker.remaining_secs(pair).await
    }
}

// ============================================================
// 5) Helpers de unidade (Gwei/Ether)
// ============================================================
#[inline]
pub fn gwei(n: u64) -> U256 {
    U256::from(n) * U256::from(1_000_000_000u64)
}

#[inline]
pub fn ether(n: u64) -> U256 {
    U256::from(n) * U256::exp10(18)
}