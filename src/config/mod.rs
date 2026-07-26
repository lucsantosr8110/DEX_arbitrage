use crate::core::types::RiskConfig;
use anyhow::{Context, Result};
use ethers::types::Address;
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeSet, HashMap}, fs, path::PathBuf, sync::Arc,
    time::{Instant, Duration, SystemTime}
};
use tokio::sync::{Mutex, RwLock};
use tracing::{debug, error, info, warn};
use std::path::Path;
use std::default::Default;
use regex::Regex;

pub mod token_cache;

// ============================================================
// Estruturas de Configuração Base
// ============================================================

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct TokenMetadata {
    pub symbol: String,
    pub name: Option<String>,
    pub decimals: Option<u8>,
    pub coingecko_id: Option<String>,
    pub category: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PairsConfig {
    pub enabled: bool,
    pub tokens: HashMap<String, String>,
    pub metadata: HashMap<String, TokenMetadata>,
    pub monitor: Vec<String>,

    /// Allowlist de tokens cujos pools foram verificados com liquidez executável.
    /// Um par de `monitor` só é cotado se ambas as pontas estiverem aqui.
    /// Vazio = usa o default seguro do radar (stables + bluechips). Preencher para
    /// experimentos com pares menos concorridos (ex.: LINK, CRV, UNI), depois de
    /// confirmar o pool on-chain. O gate de reciprocidade a notional continua
    /// filtrando o que não aguentar o tamanho de trade.
    #[serde(default)]
    pub liquidity_allowlist: Vec<String>,

    // 🔥 Compatível com seu TOML
    #[serde(default)]
    pub auto_discovery: bool,

    #[serde(default)]
    pub refresh_interval: u64,
}

impl Default for PairsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            tokens: HashMap::new(),
            metadata: HashMap::new(),
            monitor: Vec::new(),
            liquidity_allowlist: Vec::new(),
            auto_discovery: false,
            refresh_interval: 30,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct WrapperConfig {
    pub enabled: bool,
    pub address: String,
    #[serde(default)] pub owner_address: Option<String>,
    #[serde(default)] pub executor_allowed: bool,
    #[serde(default)] pub auto_authorize_executor: bool,
    #[serde(default)] pub authorization_check_enabled: bool,
    #[serde(default)] pub healthcheck_interval_sec: u32,
    #[serde(default)] pub max_failed_calls: u32,
    #[serde(default)] pub auto_recover_on_fail: bool,
    #[serde(default)] pub log_reverts: bool,
    #[serde(default)] pub contract_type: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ValidationConfig {
    /// Ativa paper validation (eth_call + balance delta, sem envio).
    /// Também respeita env `PAPER_VALIDATION=1`.
    #[serde(default)]
    pub paper_enabled: bool,
    /// Quando true, torna fisicamente impossível enviar TX (early-return no send).
    #[serde(default)]
    pub dry_run_only: bool,
    /// CSV de amostras (escrito por task async, fora do hot path do radar).
    #[serde(default = "default_paper_csv")]
    pub csv_path: String,
    /// A cada N amostras, emite resumo agregado (p50/p95).
    #[serde(default = "default_summary_window")]
    pub summary_window: u64,
    /// Endereço `from` para eth_call (público; nunca keypair).
    /// Alias `paper_from_address`. Env `PAPER_FROM` tem prioridade.
    #[serde(default, alias = "paper_from_address")]
    pub paper_from: String,
    /// Em paper: permite state override mínimo em eth_call (simulação apenas).
    /// Nunca habilita send. Default true.
    #[serde(default = "default_paper_state_overrides")]
    pub paper_state_overrides: bool,
    /// Spread mínimo (%) **só para OBSERVAÇÃO paper**. Não gateia execução.
    /// `None` = usa o mesmo valor de `arbitrage.min_spread_percent` (comportamento antigo).
    /// Em paper, tipicamente menor que o min_spread de execução (ex.: 0.05 vs 0.50).
    #[serde(default)]
    pub observe_min_spread: Option<f64>,
    /// Máximo de eth_calls paper por ciclo de preços (evita spam RPC).
    #[serde(default = "default_observe_max_per_cycle")]
    pub observe_max_per_cycle: u64,
    /// Replay histórico (paper-only): varrer blocos e medir edges triangulares.
    #[serde(default)]
    pub replay: ReplayConfig,
}

/// Janela contígua de blocos para replay denso (`step=1`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ReplayWindow {
    pub from: u64,
    pub to: u64,
    /// Rótulo curto (ex.: `us_open_high_vol`).
    #[serde(default)]
    pub label: String,
    /// Perfil documentado (alta-vol / evento / controle).
    #[serde(default)]
    pub profile: String,
}

/// Scan de blocos históricos para calibrar triangular (eth_call@blockTag).
/// Só ativo com observation_active (paper). Requer archive node.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ReplayConfig {
    #[serde(default)]
    pub enabled: bool,
    /// Inclusive. `None`/`0` + auto_us_session → resolve horário ativo.
    /// Ignorado quando `windows` não-vazio (prioridade).
    #[serde(default)]
    pub block_from: Option<u64>,
    #[serde(default)]
    pub block_to: Option<u64>,
    /// Janelas contíguas. Se presente e não-vazio, tem prioridade sobre
    /// `block_from`/`block_to`/`auto_us_session`.
    #[serde(default)]
    pub windows: Vec<ReplayWindow>,
    /// Amostrar 1 a cada `step` blocos. Dense = 1 (todo bloco).
    #[serde(default = "default_replay_step")]
    pub step: u64,
    /// Cap de eth_calls **só** nos blocos com edge (RPC cost). Scan de
    /// cycle_rate roda em todos os blocos amostrados.
    #[serde(default = "default_replay_max_eth_calls")]
    pub max_eth_calls: u64,
    #[serde(default = "default_replay_csv")]
    pub csv_path: String,
    /// Se from/to vazios e sem windows: última sessão US cash.
    #[serde(default = "default_true_replay_auto")]
    pub auto_us_session: bool,
    /// Pace mínimo entre batches RPC (ms). 0 = só o UltraRateLimiter global.
    #[serde(default = "default_replay_min_interval_ms")]
    pub min_interval_ms: u64,
}

fn default_replay_step() -> u64 {
    1
}
fn default_replay_max_eth_calls() -> u64 {
    40
}
fn default_replay_csv() -> String {
    "audits/replay_scan.csv".into()
}
fn default_true_replay_auto() -> bool {
    true
}
fn default_replay_min_interval_ms() -> u64 {
    0
}

impl Default for ReplayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            block_from: None,
            block_to: None,
            windows: Vec::new(),
            step: default_replay_step(),
            max_eth_calls: default_replay_max_eth_calls(),
            csv_path: default_replay_csv(),
            auto_us_session: true,
            min_interval_ms: default_replay_min_interval_ms(),
        }
    }
}

fn default_paper_csv() -> String {
    "audits/paper_validation.csv".into()
}
fn default_summary_window() -> u64 {
    50
}
fn default_observe_max_per_cycle() -> u64 {
    5
}
fn default_paper_state_overrides() -> bool {
    true
}

impl Default for ValidationConfig {
    fn default() -> Self {
        Self {
            paper_enabled: false,
            dry_run_only: false,
            csv_path: default_paper_csv(),
            summary_window: default_summary_window(),
            paper_from: String::new(),
            paper_state_overrides: default_paper_state_overrides(),
            observe_min_spread: None,
            observe_max_per_cycle: default_observe_max_per_cycle(),
            replay: ReplayConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AnalyticsConfig {
    pub enabled: bool,
    #[serde(default)] pub export_csv: bool,
    #[serde(default)] pub export_path: String,
    #[serde(default)] pub save_interval_sec: u64,
    #[serde(default)] pub include_gas_costs: bool,
    #[serde(default)] pub include_slippage: bool,
    #[serde(default)] pub include_profitability: bool,
    #[serde(default)] pub daily_summary_enabled: bool,
    #[serde(default)] pub weekly_summary_enabled: bool,
    #[serde(default)] pub report_currency: String,
    #[serde(default)] pub dashboard: AnalyticsDashboardConfig,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AnalyticsDashboardConfig {
    pub enabled: bool,
    #[serde(default)] pub port: u16,
    #[serde(default)] pub auto_open_browser: bool,
    #[serde(default)] pub refresh_interval_sec: u64,
    #[serde(default)] pub theme: String,
    #[serde(default)] pub show_flashloan_stats: bool,
    #[serde(default)] pub show_profit_chart: bool,
    #[serde(default)] pub show_rpc_health: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct AlertsConfig {
    pub enabled: bool,
    #[serde(default)] pub threshold_profit_usd: f64,
    #[serde(default)] pub threshold_loss_usd: f64,
    #[serde(default)] pub alert_on_rpc_fail: bool,
    #[serde(default)] pub alert_on_revert: bool,
    #[serde(default)] pub alert_on_liquidity_drop: bool,
    #[serde(default)] pub alert_on_profit_spike: bool,
    #[serde(default)] pub cooldown_seconds: u64,
    #[serde(default)] pub send_to_telegram: bool,
    #[serde(default)] pub send_to_log: bool,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DebugConfig {
    pub enabled: bool,
    #[serde(default)] pub show_flashloan_steps: bool,
    #[serde(default)] pub show_tx_input_data: bool,
    #[serde(default)] pub log_contract_calls: bool,
    #[serde(default)] pub trace_failed_tx: bool,
    #[serde(default)] pub record_events: bool,
    #[serde(default)] pub save_simulation_results: bool,
    #[serde(default)] pub max_log_size_mb: u32,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct SummaryConfig {
    #[serde(default)] pub show_startup_banner: bool,
    #[serde(default)] pub show_dex_summary: bool,
    #[serde(default)] pub show_pairs_summary: bool,
    #[serde(default)] pub show_flashloan_summary: bool,
    #[serde(default)] pub show_limits: bool,
    #[serde(default)] pub log_execution_mode: bool,
    #[serde(default)] pub colorize_output: bool,
    #[serde(default)] pub compact_mode: bool,
    #[serde(default)] pub display_timestamps: bool,
}
// ===============================
// MEV (opcional) + defaults
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MevConfig {
    pub enabled: bool,
    #[serde(default)] pub relay_url: String,
    #[serde(default)] pub signer_address: String,
    #[serde(default)] pub min_tip_matic: String,
    #[serde(default)] pub target_block_offset: u64,
    #[serde(default)] pub timeout_seconds: u64,
}
impl Default for MevConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            relay_url: "https://relay.flashbots.net".to_string(),
            signer_address: String::new(),
            min_tip_matic: "0.1".to_string(),
            target_block_offset: 1,
            timeout_seconds: 10,
        }
    }
}

// ===============================
// Cooldown, rate limiting e deduplicacao
// ===============================

fn default_global_cooldown_ms() -> u64 { 8000 }
fn default_pair_cooldown_ms() -> u64 { 20000 }
fn default_strategy_cooldown_ms() -> u64 { 15000 }
fn default_revert_penalty_ms() -> u64 { 45000 }
fn default_max_concurrent_executions() -> u32 { 2 }

fn default_max_executions_per_minute() -> u32 { 6 }
fn default_min_interval_between_trades_ms() -> u64 { 10000 }
fn default_burst_size() -> u32 { 1 }

fn default_same_pair_timeout_ms() -> u64 { 15000 }
fn default_similar_profit_threshold() -> f64 { 0.1 }
fn default_remember_reverted_txs() -> bool { true }
fn default_revert_blacklist_duration_ms() -> u64 { 30000 }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CooldownConfig {
    pub enabled: bool,
    #[serde(default = "default_global_cooldown_ms")] pub global_ms: u64,
    #[serde(default = "default_pair_cooldown_ms")] 	pub pair_ms: u64,
    #[serde(default = "default_strategy_cooldown_ms")] pub strategy_ms: u64,
    #[serde(default = "default_revert_penalty_ms")]  pub revert_penalty_ms: u64,
    #[serde(default = "default_max_concurrent_executions")] pub max_concurrent_executions: u32,
}
impl Default for CooldownConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            global_ms: default_global_cooldown_ms(),
            pair_ms: default_pair_cooldown_ms(),
            strategy_ms: default_strategy_cooldown_ms(),
            revert_penalty_ms: default_revert_penalty_ms(),
            max_concurrent_executions: default_max_concurrent_executions(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionRateLimitingConfig {
    pub enabled: bool,
    #[serde(default = "default_max_executions_per_minute")] pub max_executions_per_minute: u32,
    #[serde(default = "default_min_interval_between_trades_ms")] pub min_interval_between_trades_ms: u64,
    #[serde(default = "default_burst_size")] pub burst_size: u32,
}
impl Default for ExecutionRateLimitingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_executions_per_minute: default_max_executions_per_minute(),
            min_interval_between_trades_ms: default_min_interval_between_trades_ms(),
            burst_size: default_burst_size(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArbitrageDeduplicationConfig {
    pub enabled: bool,
    #[serde(default = "default_same_pair_timeout_ms")] pub same_pair_timeout_ms: u64,
    #[serde(default = "default_similar_profit_threshold")] pub similar_profit_threshold: f64,
    #[serde(default = "default_remember_reverted_txs")] pub remember_reverted_txs: bool,
    #[serde(default = "default_revert_blacklist_duration_ms")] pub revert_blacklist_duration_ms: u64,
}
impl Default for ArbitrageDeduplicationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            same_pair_timeout_ms: default_same_pair_timeout_ms(),
            similar_profit_threshold: default_similar_profit_threshold(),
            remember_reverted_txs: default_remember_reverted_txs(),
            revert_blacklist_duration_ms: default_revert_blacklist_duration_ms(),
        }
    }
}

// ===============================
// Cooldown Manager
// ===============================

#[derive(Debug, Clone)]
pub struct CooldownManager {
    state: Arc<RwLock<CooldownState>>,
    config: CooldownConfig,
}

#[derive(Debug, Clone)]
struct CooldownState {
    last_global_execution: Option<Instant>,
    last_pair_execution: HashMap<String, Instant>,
    last_strategy_execution: HashMap<String, Instant>,
    reverted_pairs: HashMap<String, Instant>,
    executed_blocks: HashMap<u64, Instant>,
    execution_count: u32,
    execution_timestamps: Vec<Instant>,
    last_block_number: Option<u64>,
}

impl CooldownManager {
    pub fn new(config: CooldownConfig) -> Self {
        Self {
            state: Arc::new(RwLock::new(CooldownState {
                last_global_execution: None,
                last_pair_execution: HashMap::new(),
                last_strategy_execution: HashMap::new(),
                reverted_pairs: HashMap::new(),
                executed_blocks: HashMap::new(),
                execution_count: 0,
                execution_timestamps: Vec::new(),
                last_block_number: None,
            })),
            config,
        }
    }

    pub async fn can_execute(&self, pair: &str, strategy: &str, current_block: Option<u64>) -> Result<bool, String> {
        if !self.config.enabled { return Ok(true); }
        let now = Instant::now();
        let mut state = self.state.write().await;
        self.cleanup_old_data(&mut state, now);

        if let Some(block_num) = current_block {
            if state.executed_blocks.contains_key(&block_num) {
                return Err(format!("execution already recorded for block {}", block_num));
            }
            if let Some(last_block) = state.last_block_number {
                if block_num == last_block {
                    return Err(format!("same block {} - wait", block_num));
                }
            }
        }

        let executions_last_minute = state
            .execution_timestamps
            .iter()
            .filter(|&&ts| now.duration_since(ts) < Duration::from_secs(60))
            .count();

        if executions_last_minute >= self.config.max_concurrent_executions as usize {
            return Err(format!(
                "minute execution limit reached: {}/{}",
                executions_last_minute, self.config.max_concurrent_executions
            ));
        }

        if let Some(last_global) = state.last_global_execution {
            let elapsed = now.duration_since(last_global);
            if elapsed < Duration::from_millis(self.config.global_ms) {
                let remain = self.config.global_ms.saturating_sub(elapsed.as_millis() as u64);
                return Err(format!("global cooldown {} ms", remain));
            }
        }

        if let Some(revert_time) = state.reverted_pairs.get(pair) {
            let elapsed = now.duration_since(*revert_time);
            if elapsed < Duration::from_millis(self.config.revert_penalty_ms) {
                let remain = self.config.revert_penalty_ms.saturating_sub(elapsed.as_millis() as u64);
                return Err(format!("revert penalty for {}: {} ms", pair, remain));
            } else {
                state.reverted_pairs.remove(pair);
            }
        }

        if let Some(last_pair) = state.last_pair_execution.get(pair) {
            let elapsed = now.duration_since(*last_pair);
            if elapsed < Duration::from_millis(self.config.pair_ms) {
                let remain = self.config.pair_ms.saturating_sub(elapsed.as_millis() as u64);
                return Err(format!("pair cooldown {}: {} ms", pair, remain));
            }
        }

        if let Some(last_strategy) = state.last_strategy_execution.get(strategy) {
            let elapsed = now.duration_since(*last_strategy);
            if elapsed < Duration::from_millis(self.config.strategy_ms) {
                let remain = self.config.strategy_ms.saturating_sub(elapsed.as_millis() as u64);
                return Err(format!("strategy cooldown {}: {} ms", strategy, remain));
            }
        }
        Ok(true)
    }

    pub async fn record_execution(&self, pair: &str, strategy: &str, block_number: Option<u64>) {
        let now = Instant::now();
        let mut state = self.state.write().await;
        state.last_global_execution = Some(now);
        state.last_pair_execution.insert(pair.to_string(), now);
        state.last_strategy_execution.insert(strategy.to_string(), now);
        state.execution_timestamps.push(now);
        state.execution_count += 1;
        if let Some(block_num) = block_number {
            state.executed_blocks.insert(block_num, now);
            state.last_block_number = Some(block_num);
        }
        state.execution_timestamps.retain(|&ts| now.duration_since(ts) < Duration::from_secs(120));
    }

    fn cleanup_old_data(&self, state: &mut CooldownState, now: Instant) {
        state.executed_blocks.retain(|_, t| now.duration_since(*t) < Duration::from_secs(300));
        state.last_pair_execution.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
        state.last_strategy_execution.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
        state.reverted_pairs.retain(|_, t| now.duration_since(*t) < Duration::from_secs(600));
    }

    pub async fn record_revert(&self, pair: &str) {
        let now = Instant::now();
        let mut state = self.state.write().await;
        state.reverted_pairs.insert(pair.to_string(), now);
        debug!("revert recorded for {} (penalty {} ms)", pair, self.config.revert_penalty_ms);
    }

    pub async fn get_stats(&self) -> CooldownStats {
        let state = self.state.read().await;
        let now = Instant::now();
        let executions_last_minute = state
            .execution_timestamps
            .iter()
            .filter(|&&ts| now.duration_since(ts) < Duration::from_secs(60))
            .count();
        CooldownStats {
            total_executions: state.execution_count,
            executions_last_minute,
            active_cooldowns: state.last_pair_execution.len(),
            reverted_pairs: state.reverted_pairs.len(),
            blocked_blocks: state.executed_blocks.len(),
            last_block_executed: state.last_block_number,
        }
    }

    pub async fn reset(&self) {
        let mut state = self.state.write().await;
        *state = CooldownState {
            last_global_execution: None,
            last_pair_execution: HashMap::new(),
            last_strategy_execution: HashMap::new(),
            reverted_pairs: HashMap::new(),
            executed_blocks: HashMap::new(),
            execution_count: 0,
            execution_timestamps: Vec::new(),
            last_block_number: None,
        };
    }
}

#[derive(Debug, Clone)]
pub struct CooldownStats {
    pub total_executions: u32,
    pub executions_last_minute: usize,
    pub active_cooldowns: usize,
    pub reverted_pairs: usize,
    pub blocked_blocks: usize,
    pub last_block_executed: Option<u64>,
}

// ===============================
// Flashloan (compatibilidade)
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FlashloanMinimumAmountsConfig {
    pub enabled: bool,
    pub auto_adjust: bool,
    pub validation_enabled: bool,
}
impl Default for FlashloanMinimumAmountsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            auto_adjust: true,
            validation_enabled: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FlashloanConfig {
    pub enabled: bool,
    #[serde(default)] pub max_routes: u32,
    #[serde(default)] pub max_depth: u32,
    #[serde(default)] pub capital_usd: f64,
    #[serde(default)] pub min_profit_usd: Option<f64>,
    #[serde(default)] pub wrapper_address: Option<String>,
    #[serde(default)] pub executor_address: Option<String>,
    #[serde(default)] pub aave_pool_address: Option<String>,
    #[serde(default)] pub max_amount_usdc: Option<f64>,
    #[serde(default)] pub flashloan_asset: Option<String>,
    #[serde(default)] pub flashloan_asset_address: Option<String>,
    #[serde(default)] pub flashloan_decimals: Option<u8>,
    #[serde(default)] pub max_amount_wmatic: Option<f64>,
    #[serde(default)] pub slippage_bps: Option<u32>,
    #[serde(default)] pub premium_multiplier: Option<f64>,
    #[serde(default)] pub gas_overhead: Option<u64>,
    #[serde(default)] pub provider: Option<String>,
    #[serde(default)] pub preferred_mode: Option<String>,
    #[serde(default)] pub auto_failover: Option<bool>,
    #[serde(default)] pub track_metrics: Option<bool>,
    #[serde(default)] pub simulate_before_execute: Option<bool>,
    #[serde(default)] pub revert_on_negative_profit: Option<bool>,
    #[serde(default)] pub minimum_amounts: FlashloanMinimumAmountsConfig,
    #[serde(default)] pub slippage_tolerance_pct: Option<f64>,

    /// Teto de segurança para premium do provedor, em bps. A execução recusa
    /// config cujo `fee_pct` exceda este valor.
    #[serde(default)] pub max_premium_bps: Option<u32>,

    // NEW: fee percentage charged by flashloan provider (e.g. 0.0009 = 0.09%)
    #[serde(default)] pub fee_pct: Option<f64>,
}
impl Default for FlashloanConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_routes: 1,
            max_depth: 1,
            capital_usd: 100.0,
            min_profit_usd: Some(0.10),
            wrapper_address: None,
            executor_address: None,
            aave_pool_address: None,
            max_amount_usdc: None,
            flashloan_asset: None,
            flashloan_asset_address: None,
            flashloan_decimals: None,
            max_amount_wmatic: None,
            slippage_bps: Some(50),
            premium_multiplier: Some(1.0),
            gas_overhead: Some(100000),
            provider: None,
            preferred_mode: None,
            auto_failover: Some(false),
            track_metrics: Some(true),
            simulate_before_execute: Some(true),
            revert_on_negative_profit: Some(true),
            minimum_amounts: FlashloanMinimumAmountsConfig::default(),
            slippage_tolerance_pct: Some(0.5),
            max_premium_bps: Some(10),
            fee_pct: Some(0.0005),
        }
    }
}

// ===============================
// General / Logging / Network / Wallet / Executor / Dex
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GeneralConfig {
    pub version: Option<String>,
    pub env_source: Option<String>,
    pub intermediate_tokens: Vec<String>,
    pub max_parallel_requests: u32,
    pub health_check_interval: u32,
    pub shutdown_timeout: u32,
    #[serde(default)] pub enable_frontend: Option<bool>,
    #[serde(default)] pub expand_env: bool,
}
impl Default for GeneralConfig {
    fn default() -> Self {
        Self {
            version: Some("v4.8.7-REAL-MICRO".to_string()),
            env_source: Some(".env".to_string()),
            intermediate_tokens: vec!["WMATIC".to_string(), "USDC".to_string(), "DAI".to_string()],
            max_parallel_requests: 200,
            health_check_interval: 1,
            shutdown_timeout: 300,
            enable_frontend: Some(false),
            expand_env: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LoggingLevels {
    pub core: String,
    pub arbitrage: String,
    pub execution: String,
    pub flashloan: String,
    pub network: String,
    pub risk: String,
}
impl Default for LoggingLevels {
    fn default() -> Self {
        Self {
            core: "info".to_string(),
            arbitrage: "info".to_string(),
            execution: "info".to_string(),
            flashloan: "warn".to_string(),
            network: "warn".to_string(),
            risk: "info".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: String,
    pub file_enabled: bool,
    pub file_path: String,
    pub max_file_size: String,
    pub max_files: u32,
    pub compress_logs: bool,
    pub rust_log: Option<String>,
    pub levels: LoggingLevels,
}
impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            format: "compact".to_string(),
            file_enabled: true,
            file_path: "logs/bot.log".to_string(),
            max_file_size: "20MB".to_string(),
            max_files: 3,
            compress_logs: true,
            rust_log: Some("warn".to_string()),
            levels: LoggingLevels::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LogConfig {
    #[serde(default)] pub show_opportunities: bool,
    #[serde(default)] pub show_best_paths: bool,
    #[serde(default)] pub show_rejections: bool,
    #[serde(default)] pub show_profit_estimates: bool,
    #[serde(default)] pub show_gas_details: bool,
    #[serde(default)] pub show_dex_responses: bool,
    #[serde(default)] pub export_prices_csv: bool,
    #[serde(default)] pub export_trades_csv: bool,
    /// Top-N spreads (por Spread% do TUI, desc) logados por scan no `[TOPSPREAD]`.
    /// Revela cycle_rate real bidirecional + TVL de cada pool do melhor 2-hop.
    /// 0 = desliga. Default 5.
    #[serde(default = "default_top_spreads_n")] pub top_spreads_n: usize,
}
fn default_top_spreads_n() -> usize { 5 }
impl Default for LogConfig {
    fn default() -> Self {
        Self {
            show_opportunities: true,
            show_best_paths: false,
            show_rejections: true,
            show_profit_estimates: true,
            show_gas_details: false,
            show_dex_responses: false,
            export_prices_csv: false,
            export_trades_csv: false,
            top_spreads_n: default_top_spreads_n(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct NetworkConfig {
    pub name: String,
    pub chain_id: u32,
    pub rpc_url: String,
    pub ws_url: Option<String>,
    pub block_delay_max: u64,
    pub rpc_endpoints: Option<Vec<String>>,
    pub ws_endpoints: Option<Vec<String>>,
    pub timeout_ms: u64,
    pub max_reorg_depth: u32,
    pub chain_monitoring: bool,
    #[serde(default)] pub max_pending_requests: u32,
    #[serde(default)] pub rotate_on_error: bool,
    #[serde(default)] pub fallback_timeout_ms: u64,
    #[serde(default)] pub health_check_interval: u32,
    #[serde(default)] pub consecutive_failures_before_switch: u32,
    #[serde(default)] pub enable_request_batching: bool,
    #[serde(default)] pub batch_size: u32,
    #[serde(default)] pub max_concurrent_requests: u32,
    #[serde(default)] pub connection_pool_size: u32,
    #[serde(default)] pub keep_alive: bool,
    #[serde(default)] pub compression: bool,
}
impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            name: "polygon".to_string(),
            chain_id: 137,
            rpc_url: "https://polygon-rpc.com".to_string(),
            ws_url: None,
            block_delay_max: 1,
            rpc_endpoints: None,
            ws_endpoints: None,
            timeout_ms: 2000,
            max_reorg_depth: 32,
            chain_monitoring: true,
            max_pending_requests: 512,
            rotate_on_error: true,
            fallback_timeout_ms: 2000,
            health_check_interval: 15,
            consecutive_failures_before_switch: 2,
            enable_request_batching: true,
            batch_size: 50,
            max_concurrent_requests: 100,
            connection_pool_size: 20,
            keep_alive: true,
            compression: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WalletMultisigConfig {
    pub enabled: bool,
    pub address: String,
    pub threshold: u32,
}
impl Default for WalletMultisigConfig {
    fn default() -> Self {
        Self { enabled: false, address: String::new(), threshold: 0 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WalletConfig {
    pub private_key: String,
    pub address: String,
    pub min_balance_eth: String,
    pub max_gas_price_gwei: u64,
    pub auto_sweep: bool,
    pub sweep_threshold: String,
    pub max_tx_value: String,
    pub multisig: WalletMultisigConfig,
    #[serde(default)] pub max_tx_fee_usd: f64,
}
impl Default for WalletConfig {
    fn default() -> Self {
        Self {
            private_key: String::new(),
            address: "0x0000000000000000000000000000000000000000".to_string(),
            min_balance_eth: "0.001".to_string(),
            max_gas_price_gwei: 300,
            auto_sweep: false,
            sweep_threshold: "0.01".to_string(),
            max_tx_value: "100000.0".to_string(),
            multisig: WalletMultisigConfig::default(),
            max_tx_fee_usd: 5.0,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutorConfig {
    pub address: String,
    pub validate_signature_hash: bool,
    pub max_flashloan_premium: u64,
    pub contract_type: String,
    pub timeout_seconds: u32,
    #[serde(default)] pub use_direct_calls: bool,
    #[serde(default)] pub skip_wrapper_validation: bool,
}
impl Default for ExecutorConfig {
    fn default() -> Self {
        Self {
            address: "0x0000000000000000000000000000000000000000".to_string(),
            validate_signature_hash: true,
            max_flashloan_premium: 0,
            contract_type: "StandardExecutor".to_string(),
            timeout_seconds: 300,
            use_direct_calls: true,
            skip_wrapper_validation: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct DexEntry {
    pub name: String,
    pub router_address: String,
    #[serde(default)] pub factory_address: Option<String>,
    #[serde(default)] pub quoter_address: Option<String>,
    #[serde(default)] pub approval_type: Option<u8>,
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub priority: Option<u32>,
    #[serde(default)] pub max_slippage: Option<f64>,
    #[serde(default)] pub fee_tier: Option<u32>,
    /// Gate B2 de TVL específico deste venue (USD). `None` = usa o global
    /// `arbitrage.min_liquidity`. Estava no TOML mas não existia aqui: os valores
    /// por DEX eram descartados em silêncio e só o global valia.
    #[serde(default)] pub liquidity_threshold_usd: Option<f64>,
    #[serde(default)] pub extra: HashMap<String, toml::Value>,
}

// ===============================
// Arbitrage Config - COMPLETO CORRIGIDO
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArbitrageTriangularConfig {
    pub enabled: bool,
    pub max_hops: u32,
    pub min_triangle_profit: String,
    /// Só ciclos com as 3 pernas no mesmo venue. Default **false** = cross-DEX
    /// (melhor edge por hop across venues) — desenho original do bot.
    #[serde(default = "default_false")]
    pub intra_dex_only: bool,
    /// Venues para triangular intra-DEX.
    #[serde(default = "default_tri_venues")]
    pub venues: Vec<String>,
    /// Âncoras do ciclo (flashloanáveis / profundos). Preferir USDC como start.
    #[serde(default = "default_tri_anchors")]
    pub anchors: Vec<String>,
    /// Mid-caps como perna do meio (shortlist liquidity-gated).
    #[serde(default = "default_tri_midcaps")]
    pub midcaps: Vec<String>,
}

fn default_false() -> bool {
    false
}
fn default_tri_venues() -> Vec<String> {
    vec!["UniswapV3".into()]
}
fn default_tri_anchors() -> Vec<String> {
    vec!["USDC".into(), "WETH".into(), "WMATIC".into()]
}
fn default_tri_midcaps() -> Vec<String> {
    vec!["LINK".into(), "LDO".into(), "UNI".into()]
}

impl Default for ArbitrageTriangularConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_hops: 3,
            min_triangle_profit: "0.10".to_string(),
            intra_dex_only: false,
            venues: default_tri_venues(),
            anchors: default_tri_anchors(),
            midcaps: default_tri_midcaps(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArbitrageCrossDexConfig {
    pub enabled: bool,
    pub min_cross_profit: String,
    pub max_dex_count: u32,
}

impl Default for ArbitrageCrossDexConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            min_cross_profit: "0.10".to_string(),
            max_dex_count: 2,
        }
    }
}

/// Guardas executáveis da rota. Aplicadas no gate de liquidez e imediatamente
/// antes de simulação/envio.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RouteValidationConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_route_max_hops")]
    pub max_hops: u32,
    #[serde(default)]
    pub min_liquidity_per_hop: String,
    #[serde(default)]
    pub reject_high_slippage_routes: bool,
    /// Percentual para rota inteira; ex.: `1.2` = 120 bps.
    #[serde(default = "default_route_max_cumulative_slippage")]
    pub max_cumulative_slippage: f64,
    #[serde(default)]
    pub block_same_dex_consecutive: bool,
}

fn default_true() -> bool { true }
fn default_route_max_hops() -> u32 { 3 }
fn default_route_max_cumulative_slippage() -> f64 { 2.0 }

impl Default for RouteValidationConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_hops: default_route_max_hops(),
            min_liquidity_per_hop: "0".into(),
            reject_high_slippage_routes: true,
            max_cumulative_slippage: default_route_max_cumulative_slippage(),
            block_same_dex_consecutive: false,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ArbitrageConfig {
    pub enabled: bool,
    #[serde(default)] pub executor_address: Option<String>,
    #[serde(default)] pub min_profit_threshold_usd: Option<f64>,
    #[serde(default)] pub min_profit_percent: Option<String>,
    pub min_profit_absolute: String,
    pub min_spread_percent: String,
    pub advanced_filters_enabled: bool,
    pub min_liquidity: String,
    pub max_slippage: String,
    pub default_trade_amount: String,
    pub max_position_size: String,
    pub max_gas_cost_ratio: String,
    pub simulate_prices: Option<bool>,
    pub execution_mode: Option<String>,
    pub safety_margin: String,
    pub cooldown_seconds: u32,
    pub max_path_length: u32,
    pub min_volume_24h: String,
    pub triangular: ArbitrageTriangularConfig,
    pub cross_dex: ArbitrageCrossDexConfig,
    #[serde(default)] pub route_validation: RouteValidationConfig,
    #[serde(default)] pub deduplication: ArbitrageDeduplicationConfig,
}

impl Default for ArbitrageConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            executor_address: None,
            min_profit_threshold_usd: Some(0.10),
            min_profit_percent: Some("0.05".to_string()),
            min_profit_absolute: "0.10".to_string(),
            min_spread_percent: "0.05".to_string(),
            advanced_filters_enabled: true,
            min_liquidity: "5000.0".to_string(),
            max_slippage: "0.5".to_string(),
            default_trade_amount: "25.0".to_string(),
            max_position_size: "100.0".to_string(),
            max_gas_cost_ratio: "0.05".to_string(),
            simulate_prices: Some(true),
            execution_mode: Some("direct".to_string()),
            safety_margin: "0.0".to_string(),
            cooldown_seconds: 10,
            max_path_length: 3,
            min_volume_24h: "10000.0".to_string(),
            triangular: ArbitrageTriangularConfig::default(),
            cross_dex: ArbitrageCrossDexConfig::default(),
            route_validation: RouteValidationConfig::default(),
            deduplication: ArbitrageDeduplicationConfig::default(),
        }
    }
}
// ===============================
// Gas config (Polygon) + defaults
// ===============================

fn default_cache_ttl() -> u64 { 30 }
fn default_dynamic_multiplier() -> f64 { 1.25 }
fn default_max_priority_gwei() -> f64 { 300.0 }
fn default_min_priority_gwei() -> f64 { 5.0 }
fn default_use_polygon_oracle() -> bool { true }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GasStationsConfig {
    pub etherchain: bool,
    pub blocknative: bool,
    pub pokt: bool,
    /// URL do Polygon Gas Oracle (M20). Parametrizada p/ não consultar oracle
    /// errado em outra chain silenciosamente.
    #[serde(default = "default_polygon_oracle_url")]
    pub polygon_oracle_url: String,
}
fn default_polygon_oracle_url() -> String {
    "https://gasstation.polygon.technology/v2".to_string()
}
impl Default for GasStationsConfig {
    fn default() -> Self {
        Self {
            etherchain: false,
            blocknative: false,
            pokt: false,
            polygon_oracle_url: default_polygon_oracle_url(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GasOptimizationConfig {
    pub auto_adjust: bool,
    pub min_gas_savings: f64,
    pub max_gas_price_impact: f64,
}
impl Default for GasOptimizationConfig {
    fn default() -> Self {
        Self {
            auto_adjust: true,
            min_gas_savings: 0.01,
            max_gas_price_impact: 0.25,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct GasConfig {
    pub max_gwei: u64,
    pub max_gas_limit: u64,
    pub mode: String,
    pub default_gas_price_gwei: f64,
    pub priority_gwei: f64,
    pub fallback_strategy: String,
    pub prediction_window: u64,
    pub default_gas_limit: u64,
    pub gas_buffer_percent: u32,
    pub gas_cost_limit_usd: f64,
    pub eip1559_enabled: bool,
    pub stations: GasStationsConfig,
    pub optimization: GasOptimizationConfig,
    #[serde(default = "default_use_polygon_oracle")] pub use_polygon_oracle: bool,
    #[serde(default = "default_cache_ttl")]          pub cache_ttl: u64,
    #[serde(default = "default_dynamic_multiplier")] pub dynamic_multiplier: f64,
    #[serde(default = "default_max_priority_gwei")]  pub max_priority_gwei: f64,
    #[serde(default)] pub auto_adjust: bool,
    #[serde(default)] pub auto_bargain: bool,
    #[serde(default)] pub max_fee_multiplier: Option<f64>,
    #[serde(default = "default_min_priority_gwei")]  pub min_priority_gwei: f64,
    /// Consumo de gás ESPERADO da rota (não o teto). 0 = cai em `default_gas_limit`.
    /// Usar o teto para precificar custo infla o hurdle de lucro.
    #[serde(default)] pub estimated_gas_units: u64,
}
impl Default for GasConfig {
    fn default() -> Self {
        Self {
            max_gwei: 300,
            max_gas_limit: 1_500_000,
            mode: "dynamic".to_string(),
            default_gas_price_gwei: 100.0,
            priority_gwei: 150.0,
            fallback_strategy: "average".to_string(),
            prediction_window: 10,
            default_gas_limit: 1_000_000,
            gas_buffer_percent: 10,
            gas_cost_limit_usd: 5.0,
            eip1559_enabled: true,
            stations: GasStationsConfig::default(),
            optimization: GasOptimizationConfig::default(),
            use_polygon_oracle: true,
            cache_ttl: default_cache_ttl(),
            dynamic_multiplier: default_dynamic_multiplier(),
            max_priority_gwei: default_max_priority_gwei(),
            auto_adjust: true,
            auto_bargain: false,
            max_fee_multiplier: Some(1.5),
            min_priority_gwei: default_min_priority_gwei(),
            // flashloan Aave + 3 swaps ≈ 400k gás queimado
            estimated_gas_units: 400_000,
        }
    }
}

// ===============================
// Execution Config - COMPLETO CORRIGIDO
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionSimulationConfig {
    pub enabled: bool,
    pub max_simulations: u32,
    pub simulation_timeout: u64,
    pub revert_on_fail: bool,
}

impl Default for ExecutionSimulationConfig {
    fn default() -> Self {
        Self { 
            enabled: true, 
            max_simulations: 2, 
            simulation_timeout: 2500, 
            revert_on_fail: true 
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionBackrunProtectionConfig {
    pub enabled: bool,
    pub max_slippage_increase: f64,
    pub min_block_confirmations: u32,
}

impl Default for ExecutionBackrunProtectionConfig {
    fn default() -> Self {
        Self { 
            enabled: true, 
            max_slippage_increase: 0.05, 
            min_block_confirmations: 2 
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionPriorityConfig {
    #[serde(default)] pub dynamic_mode: bool,
    #[serde(default)] pub gas_boost_threshold: f64,
    #[serde(default)] pub adaptive_fee_increment: f64,
    #[serde(default)] pub auto_reprice_failed_tx: bool,
    #[serde(default)] pub fallback_mode: String,
}

impl Default for ExecutionPriorityConfig {
    fn default() -> Self {
        Self {
            dynamic_mode: true,
            gas_boost_threshold: 1.2,
            adaptive_fee_increment: 0.15,
            auto_reprice_failed_tx: true,
            fallback_mode: "safe".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionLoggingConfig {
    #[serde(default)] pub enable_tx_traces: bool,
    #[serde(default)] pub save_failed_txs: bool,
    #[serde(default)] pub log_broadcasts: bool,
    #[serde(default)] pub log_confirmations: bool,
}

impl Default for ExecutionLoggingConfig {
    fn default() -> Self {
        Self {
            enable_tx_traces: true,
            save_failed_txs: true,
            log_broadcasts: true,
            log_confirmations: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExecutionConfig {
    pub skip_permission_check: bool,
    pub max_slippage_bps: u32,
    pub dry_run: bool,
    pub use_flashloan: bool,
    pub max_retries: u32,
    pub retry_delay_ms: u64,
    pub confirmation_blocks: u32,
    pub nonce_monitoring: bool,
    pub tx_timeout_seconds: u32,
    pub simulation: ExecutionSimulationConfig,
    pub backrun_protection: ExecutionBackrunProtectionConfig,
    #[serde(default)] pub priority: Option<ExecutionPriorityConfig>,
    #[serde(default)] pub logging: Option<ExecutionLoggingConfig>,
    #[serde(default)] pub auto_replace_tx: bool,
    #[serde(default)] pub replace_multiplier: Option<f64>,
    #[serde(default)] pub replace_max_retries: Option<u8>,
    #[serde(default)] pub force_flashloan: bool,
    #[serde(default)] pub enabled: bool,
    #[serde(default)] pub execution_mode: Option<String>,
    #[serde(default)] pub cooldown: CooldownConfig,
    #[serde(default)] pub rate_limiting: ExecutionRateLimitingConfig,

    // NEW fields for dex/impact/slippage defaults and overrides (no hardcodes in code)
    #[serde(default)] pub default_dex_fee_bps: u32,
    #[serde(default)] pub default_price_impact_bps: u32,
    #[serde(default)] pub dex_fee_bps_map: HashMap<String, u32>,
    #[serde(default)] pub dex_price_impact_bps_map: HashMap<String, u32>,
    #[serde(default)] pub hop_slippage_increase_bps: u32,
    #[serde(default)] pub safety_margin_bps: u32,
    #[serde(default)] pub estimate_base_gas_usd: f64,

    /// Buffer de drift quote→execução, em bps do notional. **Default 0.**
    ///
    /// NÃO é price impact: quotes já são impact-inclusive (ver `core::economics`).
    /// Só ligue isto se quiser pagar explicitamente por risco de latência.
    #[serde(default)] pub adverse_move_bps: u32,
}

impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
            skip_permission_check: true,
            max_slippage_bps: 50, // 50 bps = 0.5%
            dry_run: false,
            use_flashloan: false,
            max_retries: 2,
            retry_delay_ms: 2000,
            confirmation_blocks: 2,
            nonce_monitoring: true,
            tx_timeout_seconds: 180,
            simulation: ExecutionSimulationConfig::default(),
            backrun_protection: ExecutionBackrunProtectionConfig::default(),
            priority: Some(ExecutionPriorityConfig::default()),
            logging: Some(ExecutionLoggingConfig::default()),
            auto_replace_tx: true,
            replace_multiplier: Some(1.12),
            replace_max_retries: Some(5),
            force_flashloan: false,
            enabled: true,
            execution_mode: Some("direct".to_string()),
            cooldown: CooldownConfig::default(),
            rate_limiting: ExecutionRateLimitingConfig::default(),

            // sensible defaults (configurable via TOML)
            // NOTA: default_dex_fee_bps e default_price_impact_bps NÃO entram no
            // cálculo de net profit — quotes já são fee+impact-inclusive.
            // Ver core::economics. Mantidos só para overrides de simulação legada.
            default_dex_fee_bps: 30,            // 0.30%
            default_price_impact_bps: 50,       // 0.50%
            adverse_move_bps: 0,                // opt-in; ver core::economics
            dex_fee_bps_map: HashMap::new(),
            dex_price_impact_bps_map: HashMap::new(),
            hop_slippage_increase_bps: 5,       // 5 bps = 0.05% por hop adicional
            safety_margin_bps: 9800,            // 98% safety (BPS-like)
            estimate_base_gas_usd: 0.02,
        }
    }
}
// ===============================
// Optimization config (Default manual)
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OptimizationPredictiveConfig {
    #[serde(default)] pub window_size: u32,
    #[serde(default)] pub update_interval: u32,
    #[serde(default)] pub learning_rate: f64,
    #[serde(default)] pub confidence_threshold: f64,
    #[serde(default)] pub fallback_on_low_confidence: bool,
}
impl Default for OptimizationPredictiveConfig {
    fn default() -> Self {
        Self {
            window_size: 20,
            update_interval: 5,
            learning_rate: 0.05,
            confidence_threshold: 0.8,
            fallback_on_low_confidence: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OptimizationConfig {
    pub quick_filter_enabled: bool,
    #[serde(default)] pub pre_filter_opportunities: bool,
    #[serde(default)] pub adaptive_radar_mode: bool,
    #[serde(default)] pub spread_decay_window: u32,
    #[serde(default)] pub batch_processing_size: u32,
    #[serde(default)] pub ultra_fast_mode: bool,
    #[serde(default)] pub auto_tune_execution: bool,
    #[serde(default)] pub auto_retry_failed_swaps: bool,
    #[serde(default)] pub retry_limit: u32,
    pub min_spread_percent: f64,
    #[serde(default)] pub min_profit_after_gas: f64,
    #[serde(default)] pub max_gas_ratio: f64,
    #[serde(default)] pub min_profit_per_tx: f64,
    #[serde(default)] pub dynamic_slippage_enabled: bool,
    #[serde(default)] pub slippage_multiplier: f64,
    #[serde(default)] pub cooldown_after_fail_seconds: u64,
    #[serde(default)] pub predictive: Option<OptimizationPredictiveConfig>,
}
impl Default for OptimizationConfig {
    fn default() -> Self {
        Self {
            quick_filter_enabled: true,
            pre_filter_opportunities: true,
            adaptive_radar_mode: true,
            spread_decay_window: 5,
            batch_processing_size: 20,
            ultra_fast_mode: true,
            auto_tune_execution: true,
            auto_retry_failed_swaps: true,
            retry_limit: 2,
            min_spread_percent: 0.001,
            min_profit_after_gas: 0.02,
            max_gas_ratio: 1.0,
            min_profit_per_tx: 0.05,
            dynamic_slippage_enabled: true,
            slippage_multiplier: 1.05,
            cooldown_after_fail_seconds: 10,
            predictive: Some(OptimizationPredictiveConfig::default()),
        }
    }
}

// ===============================
// Telegram / Monitoring / Performance / Metrics
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TelegramCommands {
    pub enable_controls: bool,
    pub allowed_users: Vec<String>,
    pub admin_chat_id: String,
}
impl Default for TelegramCommands {
    fn default() -> Self {
        Self {
            enable_controls: false,
            allowed_users: Vec::new(),
            admin_chat_id: String::new(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct TelegramConfig {
    pub enabled: bool,
    pub bot_token: String,
    pub chat_id: String,
    pub notify_on_profit: bool,
    pub notify_on_fail: bool,
    pub notify_on_startup: bool,
    pub notify_on_opportunity: bool,
    pub notify_on_rpc_switch: bool,
    pub min_profit_notification: String,
    pub alert_cooldown: u32,
    pub commands: TelegramCommands,
}
impl Default for TelegramConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            bot_token: String::new(),
            chat_id: String::new(),
            notify_on_profit: true,
            notify_on_fail: true,
            notify_on_startup: true,
            notify_on_opportunity: false,
            notify_on_rpc_switch: true,
            min_profit_notification: "0.10".to_string(),
            alert_cooldown: 30,
            commands: TelegramCommands::default(),
        }
    }
}

fn default_monitoring_enabled() -> bool { true }
fn default_monitoring_interval() -> u64 { 30 }

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MonitoringConfig {
    #[serde(default = "default_monitoring_enabled")] pub enabled: bool,
    #[serde(default = "default_monitoring_interval")] pub interval_sec: u64,
}
impl Default for MonitoringConfig {
    fn default() -> Self { Self { enabled: true, interval_sec: 30 } }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PerformanceConfig {
    pub enabled: bool,
    #[serde(default)] pub save_interval_sec: u64,
    #[serde(default)] pub max_records: usize,
}
impl Default for PerformanceConfig {
    fn default() -> Self {
        Self { enabled: false, save_interval_sec: 60, max_records: 10000 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PrometheusConfig {
    pub enabled: bool,
    #[serde(default)] pub port: u16,
    #[serde(default)] pub endpoint: String,
}
impl Default for PrometheusConfig {
    fn default() -> Self { Self { enabled: false, port: 9090, endpoint: "/metrics".to_string() } }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct MetricsConfig {
    pub enabled: bool,
    #[serde(default)] pub port: u16,
    #[serde(default)] pub endpoint: String,
}
impl Default for MetricsConfig {
    fn default() -> Self { Self { enabled: false, port: 9100, endpoint: "/metrics".to_string() } }
}

// ===============================
// Database / Explorer / API / Emergency / Strategy / Compliance / Security / Backtesting
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DatabaseTablesConfig {
    #[serde(default)] pub trades: String,
    #[serde(default)] pub opportunities: String,
    #[serde(default)] pub performance: String,
}
impl Default for DatabaseTablesConfig {
    fn default() -> Self {
        Self {
            trades: "trades".to_string(),
            opportunities: "opportunities".to_string(),
            performance: "performance".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DatabaseConfig {
    pub enabled: bool,
    #[serde(default)] pub url: String,
    #[serde(default)] pub max_connections: u32,
    #[serde(default)] pub connection_timeout: u32,
    #[serde(default)] pub tables: DatabaseTablesConfig,
}
impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: String::new(),
            max_connections: 4,
            connection_timeout: 10,
            tables: DatabaseTablesConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExplorerMonitoringConfig {
    #[serde(default)] pub track_transactions: bool,
    #[serde(default)] pub verify_contracts: bool,
    #[serde(default)] pub monitor_gas: bool,
}
impl Default for ExplorerMonitoringConfig {
    fn default() -> Self {
        Self { track_transactions: true, verify_contracts: false, monitor_gas: false }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ExplorerConfig {
    pub enabled: bool,
    #[serde(default)] pub api_key: String,
    #[serde(default)] pub base_url: String,
    #[serde(default)] pub timeout_seconds: u32,
    #[serde(default)] pub rate_limit_rpm: u32,
    #[serde(default)] pub monitoring: ExplorerMonitoringConfig,
}
impl Default for ExplorerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            api_key: String::new(),
            base_url: String::new(),
            timeout_seconds: 10,
            rate_limit_rpm: 60,
            monitoring: ExplorerMonitoringConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ApiEndpointsConfig {
    #[serde(default)] pub health: String,
    #[serde(default)] pub metrics: String,
    #[serde(default)] pub control: String,
    #[serde(default)] pub status: String,
}
impl Default for ApiEndpointsConfig {
    fn default() -> Self {
        Self {
            health: "/health".to_string(),
            metrics: "/metrics".to_string(),
            control: "/control".to_string(),
            status: "/status".to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ApiConfig {
    pub enabled: bool,
    #[serde(default)] pub host: String,
    #[serde(default)] pub port: u16,
    #[serde(default)] pub cors_origins: Vec<String>,
    #[serde(default)] pub rate_limit: u32,
    #[serde(default)] pub auth_required: bool,
    #[serde(default)] pub endpoints: ApiEndpointsConfig,
}
impl Default for ApiConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_string(),
            port: 8080,
            cors_origins: vec!["*".to_string()],
            rate_limit: 60,
            auth_required: false,
            endpoints: ApiEndpointsConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct EmergencyConfig {
    pub enabled: bool,
    #[serde(default)] pub stop_on_error: bool,
}
impl Default for EmergencyConfig {
    fn default() -> Self { Self { enabled: true, stop_on_error: false } }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct StrategyConfig {
    #[serde(rename = "type")] pub type_str: String,
    #[serde(default)] pub use_wrapper: bool,
    #[serde(default)] pub fallback_enabled: bool,
    #[serde(default)] pub max_complexity: u32,
    #[serde(default)] pub profit_calculation: String,
    #[serde(default)] pub name: String,
    #[serde(default)] pub description: String,
    #[serde(default)] pub max_positions: u32,
}
impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            type_str: "direct".to_string(),
            use_wrapper: false,
            fallback_enabled: true,
            max_complexity: 1,
            profit_calculation: "simple".to_string(),
            name: "DirectExecution".to_string(),
            description: "Execucao com capital proprio (sem flashloan)".to_string(),
            max_positions: 1,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ComplianceConfig {
    pub enabled: bool,
    #[serde(default)] pub auditor: String,
    #[serde(default)] pub mode: String,
}
impl Default for ComplianceConfig {
    fn default() -> Self {
        Self { enabled: false, auditor: String::new(), mode: "passive".to_string() }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct SecurityConfig {
    #[serde(default)] pub tls_enabled: bool,
    #[serde(default)] pub cert_path: String,
}
impl Default for SecurityConfig {
    fn default() -> Self { Self { tls_enabled: false, cert_path: String::new() } }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct BacktestingConfig {
    pub enabled: bool,
    #[serde(default)] pub start_date: String,
    #[serde(default)] pub end_date: String,
}
impl Default for BacktestingConfig {
    fn default() -> Self { Self { enabled: false, start_date: String::new(), end_date: String::new() } }
}

// ===============================
// Cache / Rate limiting / Price feed / Detection / Radar
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CacheTypesConfig {
    #[serde(default)] pub prices: u64,
    #[serde(default)] pub pools: u64,
    #[serde(default)] pub opportunities: u64,
    #[serde(default)] pub gas: u64,
}
impl Default for CacheTypesConfig {
    fn default() -> Self {
        Self { prices: 30, pools: 30, opportunities: 10, gas: 10 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct CacheConfig {
    pub enabled: bool,
    #[serde(default)] pub max_size_mb: u32,
    #[serde(default)] pub redis_url: String,
    #[serde(default)] pub ttl_seconds: u64,
    #[serde(default)] pub max_cache_size: usize,
    #[serde(default)] pub cleanup_interval: u64,
    #[serde(default)] pub adaptive_cache_cleanup: bool,
    #[serde(default)] pub types: CacheTypesConfig,
}
// Substitua/atualize a impl Default para CacheConfig (procure pelo bloco `impl Default for CacheConfig`)
impl Default for CacheConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            max_size_mb: 128,
            // ADD: inicializa redis_url para evitar erro E0063
            redis_url: String::new(),
            ttl_seconds: 60,
            max_cache_size: 100_000,
            cleanup_interval: 30,
            adaptive_cache_cleanup: true,
            types: CacheTypesConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RateLimitingConfig {
    #[serde(default)] pub rpc_requests_per_second: u64,
    #[serde(default)] pub dex_requests_per_second: u64,
    #[serde(default)] pub batch_size: u32,
    #[serde(default)] pub max_concurrent_tasks: u32,
    #[serde(default)] pub auto_backoff: bool,
}
impl Default for RateLimitingConfig {
    fn default() -> Self {
        Self {
            rpc_requests_per_second: 50,
            dex_requests_per_second: 50,
            batch_size: 25,
            max_concurrent_tasks: 200,
            auto_backoff: true,
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PriceFeedOraclesConfig {
    #[serde(default)] pub chainlink: String,
    #[serde(default)] pub band_protocol: String,
}
impl Default for PriceFeedOraclesConfig {
    fn default() -> Self {
        Self { chainlink: String::new(), band_protocol: String::new() }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PriceFeedAggregatorsConfig {
    #[serde(default)] pub coinmarketcap: bool,
    #[serde(default)] pub coingecko: bool,
    #[serde(default)] pub defillama: bool,
}
impl Default for PriceFeedAggregatorsConfig {
    fn default() -> Self { Self { coinmarketcap: false, coingecko: true, defillama: false } }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct PriceFeedConfig {
    pub enabled: bool,
    #[serde(default)] pub update_interval: u32,
    #[serde(default)] pub max_age_seconds: u32,
    #[serde(default)] pub sources: Vec<String>,
    #[serde(default)] pub oracles: PriceFeedOraclesConfig,
    #[serde(default)] pub aggregators: PriceFeedAggregatorsConfig,
}
impl Default for PriceFeedConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            update_interval: 15,
            max_age_seconds: 60,
            sources: vec!["coingecko".to_string()],
            oracles: PriceFeedOraclesConfig::default(),
            aggregators: PriceFeedAggregatorsConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DetectionFiltersConfig {
    #[serde(default)] pub min_tvl: String,
    #[serde(default)] pub min_volume_24h: String,
    #[serde(default)] pub max_volatility: f64,
}
impl Default for DetectionFiltersConfig {
    fn default() -> Self {
        Self { min_tvl: "100000.0".to_string(), min_volume_24h: "100000.0".to_string(), max_volatility: 0.2 }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DetectionAdvancedConfig {
    #[serde(default)] pub path_discovery: bool,
    #[serde(default)] pub cycle_detection: bool,
    #[serde(default)] pub cross_dex_arbitrage: bool,
    #[serde(default)] pub multi_hop_arbitrage: bool,
}
impl Default for DetectionAdvancedConfig {
    fn default() -> Self {
        Self { path_discovery: true, cycle_detection: true, cross_dex_arbitrage: true, multi_hop_arbitrage: true }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct DetectionConfig {
    pub enabled: bool,
    #[serde(default)] pub scan_interval_ms: u64,
    #[serde(default)] pub max_opportunities_per_cycle: u32,
    #[serde(default)] pub min_profitability: String,
    #[serde(default)] pub max_gas_cost: String,
    #[serde(default)] pub filters: DetectionFiltersConfig,
    #[serde(default)] pub advanced: DetectionAdvancedConfig,
}
impl Default for DetectionConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_interval_ms: 500,
            max_opportunities_per_cycle: 100,
            min_profitability: "0.10".to_string(),
            max_gas_cost: "5.0".to_string(),
            filters: DetectionFiltersConfig::default(),
            advanced: DetectionAdvancedConfig::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RadarDebugConfig {
    #[serde(default)] pub show_price_updates: bool,
    #[serde(default)] pub show_liquidity: bool,
    #[serde(default)] pub show_path_discovery: bool,
}
impl Default for RadarDebugConfig {
    fn default() -> Self {
        Self { show_price_updates: false, show_liquidity: false, show_path_discovery: false }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RadarConfig {
    #[serde(default)] pub scan_interval_seconds: u32,
    #[serde(default)] pub timeout_ms: u64,
    #[serde(default)] pub ultrafast_enabled: bool,
    #[serde(default)] pub max_dex_parallel: u32,
    #[serde(default)] pub max_pair_parallel: u32,
    #[serde(default)] pub adaptive_interval: bool,
    #[serde(default)] pub priority_mode: bool,
    #[serde(default)] pub debug: RadarDebugConfig,
}
impl Default for RadarConfig {
    fn default() -> Self {
        Self {
            scan_interval_seconds: 1,
            timeout_ms: 1500,
            ultrafast_enabled: true,
            max_dex_parallel: 8,
            max_pair_parallel: 64,
            adaptive_interval: true,
            priority_mode: true,
            debug: RadarDebugConfig::default(),
        }
    }
}
/// Loga as chaves do TOML que o parser ignorou. Não falha o boot — só torna
/// impossível "configurar" algo e não perceber que não teve efeito.
fn report_ignored_keys(raw_toml: &str, cfg: &Config, path: &std::path::Path) {
    let ignored = Config::ignored_toml_keys(raw_toml, cfg);
    if ignored.is_empty() {
        return;
    }
    warn!(
        "⚠️ {:?}: {} chave(s) do TOML NÃO são lidas pelo parser — não têm efeito nenhum. \
         Ou o campo não existe na struct, ou está na seção errada.",
        path,
        ignored.len()
    );
    for k in &ignored {
        warn!("   ⚠️ config ignorada: {k}");
    }
}

// ===============================
// Root Config
// ===============================

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub general: GeneralConfig,
    pub logging: LoggingConfig,
    pub log: LogConfig,
    pub network: NetworkConfig,
    pub wallet: WalletConfig,
    pub executor: ExecutorConfig,
    pub dex: Vec<DexEntry>,
    pub pairs: PairsConfig,
    #[serde(skip_deserializing, default)] pub addresses: HashMap<String, Address>,
    pub flashloan: FlashloanConfig,
    pub gas: GasConfig,
    pub arbitrage: ArbitrageConfig,
    pub execution: ExecutionConfig,
    #[serde(default)] pub mev: MevConfig,
    pub optimization: OptimizationConfig,
    pub performance: PerformanceConfig,
    pub prometheus: PrometheusConfig,
    pub risk: RiskConfig,
    pub telegram: Option<TelegramConfig>,
    #[serde(default)] pub monitoring: MonitoringConfig,
    #[serde(default)] pub metrics: MetricsConfig,
    #[serde(default)] pub database: DatabaseConfig,
    #[serde(default)] pub explorer: ExplorerConfig,
    #[serde(default)] pub api: ApiConfig,
    #[serde(default)] pub emergency: EmergencyConfig,
    #[serde(default)] pub strategy: StrategyConfig,
    #[serde(default)] pub compliance: ComplianceConfig,
    #[serde(default)] pub security: SecurityConfig,
    #[serde(default)] pub backtesting: BacktestingConfig,
    #[serde(default)] pub cache: CacheConfig,
    #[serde(default)] pub rate_limiting: RateLimitingConfig,
    #[serde(default)] pub price_feed: PriceFeedConfig,
    #[serde(default)] pub detection: DetectionConfig,
    #[serde(default)] pub radar: RadarConfig,
    #[serde(default)] pub wrapper: WrapperConfig,
    #[serde(default)] pub analytics: AnalyticsConfig,
    #[serde(default)] pub validation: ValidationConfig,
    #[serde(default)] pub alerts: AlertsConfig,
    #[serde(default)] pub debug: DebugConfig,
    #[serde(default)] pub summary: SummaryConfig,
    #[serde(default)] pub min_profit_usd_threshold: f64,
    #[serde(skip)] pub cooldown_manager: Option<Arc<CooldownManager>>,
}

/// Impl config
impl Config {
    fn apply_fallbacks(&mut self) {
        if !self.monitoring.enabled { self.monitoring.enabled = true; }
        if self.monitoring.interval_sec == 0 { self.monitoring.interval_sec = 30; }

        if self.network.rpc_endpoints.is_none() {
            self.network.rpc_endpoints = Some(vec![self.network.rpc_url.clone()]);
        }
        if self.network.ws_endpoints.is_none() {
            self.network.ws_endpoints = self.network.ws_url.clone().map(|s| vec![s]);
        }

        // Flashloan permanece desativado por padrao; manter compatibilidade
        if self.flashloan.flashloan_asset.is_none() {
            self.flashloan.flashloan_asset = Some("WMATIC".to_string());
        }
        if self.flashloan.flashloan_asset_address.is_none() {
            self.flashloan.flashloan_asset_address = Some("0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270".to_string());
        }
        if self.flashloan.max_amount_wmatic.is_none() {
            self.flashloan.max_amount_wmatic = Some(10.0);
        }

        // Notional de COTAÇÃO vs notional de LUCRO.
        //
        // Os quotes são cotados em `arbitrage.default_trade_amount` (ver
        // dex::quote_amount_for_usd), mas o lucro escala por `flashloan.capital_usd`
        // quando o flashloan está ligado. Se divergirem, o gross é extrapolado com
        // um rate medido em outro tamanho — price impact do tamanho real fica de
        // fora e o lucro sai superestimado. Nada no código força a igualdade, então
        // avisar alto.
        if self.flashloan.enabled {
            let quote_notional = self
                .arbitrage
                .default_trade_amount
                .parse::<f64>()
                .unwrap_or(0.0);
            let profit_notional = self.flashloan.capital_usd;
            if quote_notional > 0.0
                && profit_notional > 0.0
                && (quote_notional - profit_notional).abs() / quote_notional > 0.01
            {
                warn!(
                    "⚠️ Notional divergente: quotes cotados a ${:.2} (arbitrage.default_trade_amount) \
                     mas lucro escalado a ${:.2} (flashloan.capital_usd). O rate medido não vale \
                     nesse tamanho — gross fica superestimado. Iguale os dois.",
                    quote_notional, profit_notional
                );
            }
        }

        if self.min_profit_usd_threshold == 0.0 {
            self.min_profit_usd_threshold = self.flashloan.min_profit_usd.unwrap_or(0.000005);
        }
        if self.arbitrage.min_profit_threshold_usd.is_none() {
            self.arbitrage.min_profit_threshold_usd = Some(self.min_profit_usd_threshold);
        }

        if self.mev.enabled && self.mev.signer_address.is_empty() {
            self.mev.signer_address = self.wallet.address.clone();
            info!("MEV signer fallback to wallet {}", self.mev.signer_address);
        }
        if self.mev.enabled {
            self.execution.execution_mode = Some("mev_bundle".to_string());
            info!("MEV enabled: execution_mode = mev_bundle");
        }

        if self.execution.cooldown.enabled {
            self.cooldown_manager = Some(Arc::new(CooldownManager::new(self.execution.cooldown.clone())));
            info!("Cooldown Manager ON: global={}ms, pair={}ms",
                self.execution.cooldown.global_ms, self.execution.cooldown.pair_ms);
        } else {
            warn!("Cooldown Manager OFF");
        }

        if self.arbitrage.cooldown_seconds < 5 {
            warn!("Cooldown too low ({}s) -> set to 10s", self.arbitrage.cooldown_seconds);
            self.arbitrage.cooldown_seconds = 10;
        }
        if self.execution.retry_delay_ms < 1000 {
            warn!("retry_delay_ms too low ({} ms) -> set to 2000 ms", self.execution.retry_delay_ms);
            self.execution.retry_delay_ms = 2000;
        }

        // ----------------------------------------------------------------------
        // 🔥 Seleção final do modo de execução (MEV / FLASHLOAN / DIRECT)
        // ----------------------------------------------------------------------

        // MEV → tem prioridade absoluta
        if self.mev.enabled {
            self.execution.execution_mode = Some("mev_bundle".to_string());
            info!("MEV ENABLED → execution_mode = mev_bundle (prioridade total)");

            // Evitar conflito quando use_flashloan = true
            if self.execution.use_flashloan {
                warn!("⚠ MEV está ativo — ignorando use_flashloan=true e forçando MEV bundle");
            }

        } else {
            // Caso MEV NÃO esteja ativo → respeitar 100% o config.toml
            if self.execution.use_flashloan {
                // Flashloan ativado pelo usuário
                self.execution.execution_mode = Some("flashloan".to_string());
                info!("Flashloan ENABLED via config.toml (use_flashloan = true)");
            } else {
                // Execução direta
                self.execution.execution_mode = Some("direct".to_string());
                info!("Flashloan DISABLED → execution_mode = direct");
            }
        }

        // Log de modo final
        info!(
            "Modo final de execução definido: {:?}",
            self.execution.execution_mode
        );
    }


    /// A partir daqui está ok

    pub fn get_cooldown_manager(&self) -> Option<Arc<CooldownManager>> {
        self.cooldown_manager.clone()
    }

    /// Chaves presentes no TOML que o parser NÃO leu.
    ///
    /// Método: TOML bruto → `Config` (parser real) → TOML. O que entra e não
    /// volta não tem campo correspondente em nenhuma struct, logo não configura
    /// nada. Usa o próprio serde como fonte de verdade em vez de uma lista
    /// manual que envelhece.
    pub fn ignored_toml_keys(raw_toml: &str, parsed: &Config) -> Vec<String> {
        fn flatten(v: &toml::Value, prefix: &str, out: &mut BTreeSet<String>) {
            match v {
                toml::Value::Table(t) => {
                    for (k, val) in t {
                        let path = if prefix.is_empty() {
                            k.clone()
                        } else {
                            format!("{prefix}.{k}")
                        };
                        match val {
                            toml::Value::Table(_) => flatten(val, &path, out),
                            toml::Value::Array(arr) => match arr.first() {
                                // array de tabelas ([[dex]]): audita o 1º item
                                Some(toml::Value::Table(_)) => {
                                    flatten(&arr[0], &format!("{path}[]"), out)
                                }
                                _ => {
                                    out.insert(path);
                                }
                            },
                            _ => {
                                out.insert(path);
                            }
                        }
                    }
                }
                _ => {
                    out.insert(prefix.to_string());
                }
            }
        }

        let Ok(input) = toml::from_str::<toml::Value>(raw_toml) else {
            return Vec::new();
        };
        let Ok(roundtrip) = toml::Value::try_from(parsed) else {
            return Vec::new();
        };

        let mut in_keys = BTreeSet::new();
        let mut out_keys = BTreeSet::new();
        flatten(&input, "", &mut in_keys);
        flatten(&roundtrip, "", &mut out_keys);

        in_keys.difference(&out_keys).cloned().collect()
    }

    pub fn from_file(path: PathBuf) -> Result<Arc<Mutex<Config>>> {
        let raw = fs::read_to_string(&path).with_context(|| format!("failed to read: {:?}", path))?;
        let cleaned = remove_bom(&raw);
        let expanded = {
            let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")?;
            re.replace_all(&cleaned, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
            }).to_string()
        };
        let mut cfg: Config = toml::from_str(&expanded)
            .with_context(|| format!("failed to parse TOML: {:?}", path))?;

        // Nenhuma chave pode ser ignorada em silêncio: sem `deny_unknown_fields`
        // (que quebraria configs existentes), uma chave com typo ou de uma seção
        // nunca implementada simplesmente não faz nada — e o operador acha que
        // configurou. Ver `report_ignored_keys`.
        report_ignored_keys(&expanded, &cfg, &path);

        for (symbol, addr_str) in &cfg.pairs.tokens {
            if let Ok(addr) = addr_str.parse::<Address>() {
                cfg.addresses.insert(symbol.clone(), addr);
            } else {
                warn!("invalid address {}: {}", symbol, addr_str);
            }
        }

        cfg.apply_fallbacks();

        // Retornar o MESMO Arc que o reload listener usa — antes era criado
        // um Arc local para o listener e retornado cfg por valor, que main.rs
        // embrulhava em OUTRO Arc. O listener atualizava o Arc órfão que
        // ninguém lia. Ver ESTADO_ATUAL.md seção 6.1.
        let cfg_arc = Arc::new(Mutex::new(cfg));
        let reload_arc = cfg_arc.clone();
        let path_clone = path.clone();
        tokio::spawn(async move {
            Config::start_reload_listener(reload_arc, path_clone).await;
        });

        Ok(cfg_arc)
    }

    pub async fn start_reload_listener(cfg: Arc<Mutex<Config>>, path: PathBuf) {
        tokio::spawn(async move {
            let mut last_mtime: Option<SystemTime> = None;
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                if let Ok(metadata) = fs::metadata(&path) {
                    if let Ok(modified) = metadata.modified() {
                        let changed = last_mtime.map_or(true, |prev| modified > prev);
                        if changed {
                            last_mtime = Some(modified);
                            match Config::reload_config(&cfg, &path).await {
                                Ok(true) => {
                                    info!("config reloaded from {:?}", path);
                                    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
                                }
                                Ok(false) => {}
                                Err(e) => error!("reload error: {:?}", e),
                            }
                        }
                    }
                }
            }
        });
    }

    async fn reload_config(cfg: &Arc<Mutex<Config>>, path: &PathBuf) -> Result<bool> {
        let raw = fs::read_to_string(path).with_context(|| format!("failed to read: {:?}", path))?;
        let cleaned = remove_bom(&raw);
        let expanded = {
            let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")?;
            re.replace_all(&cleaned, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
            }).to_string()
        };
        let mut new_cfg: Config = toml::from_str(&expanded)
            .context("failed to parse new TOML on reload")?;
        report_ignored_keys(&expanded, &new_cfg, path);

        for (symbol, addr_str) in &new_cfg.pairs.tokens {
            if let Ok(addr) = addr_str.parse::<Address>() {
                new_cfg.addresses.insert(symbol.clone(), addr);
            } else {
                warn!("invalid address {}: {}", symbol, addr_str);
            }
        }

        let mut guard = cfg.lock().await;
        new_cfg.apply_fallbacks();
        *guard = new_cfg;
        Ok(true)
    }

    pub fn reload<P: AsRef<Path>>(&mut self, path: P) -> Result<bool> {
        let raw = fs::read_to_string(&path)
            .with_context(|| format!("failed to read: {:?}", path.as_ref()))?;
        let cleaned = remove_bom(&raw);
        let expanded = {
            let re = Regex::new(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}")?;
            re.replace_all(&cleaned, |caps: &regex::Captures| {
                std::env::var(&caps[1]).unwrap_or_else(|_| caps[0].to_string())
            }).to_string()
        };
        let mut new_cfg: Config = toml::from_str(&expanded)
            .context("failed to parse new TOML on reload")?;
        report_ignored_keys(&expanded, &new_cfg, path.as_ref());

        for (symbol, addr_str) in &new_cfg.pairs.tokens {
            if let Ok(addr) = addr_str.parse::<Address>() {
                new_cfg.addresses.insert(symbol.clone(), addr);
            } else {
                warn!("invalid address {}: {}", symbol, addr_str);
            }
        }

        new_cfg.apply_fallbacks();
        *self = new_cfg;
        Ok(true)
    }

    pub fn log_start_cycle_snapshot(&self) {
        info!("===================================================");
        info!("CONFIG SNAPSHOT v4.8.7-REAL-MICRO (Direct Execution)");
        info!("===================================================");
        info!("Version: {}", self.general.version.as_deref().unwrap_or("unknown"));
        info!("Monitoring: {} ({}s)", self.monitoring.enabled, self.monitoring.interval_sec);
        info!("Dry Run: {}", self.execution.dry_run);
        info!("Auto Replace TX: {}", self.execution.auto_replace_tx);
        info!("Priority Gwei: {:.2}", self.gas.priority_gwei);
        info!("Max Gwei: {}", self.gas.max_gwei);
        info!("Min Profit (arbitrage): ${:.6}",
            self.arbitrage.min_profit_absolute.parse::<f64>().unwrap_or(0.0));
        info!("Global min profit: ${:.6}", self.min_profit_usd_threshold);
        info!("Executor: {}", self.executor.address);
        info!("Active DEX count: {}", self.dex.iter().filter(|d| d.enabled).count());
        info!("MEV Enabled: {}", self.mev.enabled);
        if self.mev.enabled {
            info!("MEV Relay: {}", self.mev.relay_url);
            info!("MEV Signer: {}", self.mev.signer_address);
            info!("MEV Min Tip: {} MATIC", self.mev.min_tip_matic);
            info!("MEV Target Block Offset: +{}", self.mev.target_block_offset);
            info!("MEV Timeout: {}s", self.mev.timeout_seconds);
        }
        info!("Cooldown Global: {} ms", self.execution.cooldown.global_ms);
        info!("Cooldown Pair: {} ms", self.execution.cooldown.pair_ms);
        info!("Cooldown Strategy: {} ms", self.execution.cooldown.strategy_ms);
        info!("Revert Penalty: {} ms", self.execution.cooldown.revert_penalty_ms);
        info!("Max concurrent executions: {}", self.execution.cooldown.max_concurrent_executions);
        info!("Cooldown Enabled: {}", self.execution.cooldown.enabled);
        info!("===================================================");
    }

    pub fn is_mev_enabled(&self) -> bool {
        self.mev.enabled && !self.mev.relay_url.is_empty() && !self.mev.signer_address.is_empty()
    }
}

// ===============================
// Utilitarios
// ===============================

fn remove_bom(content: &str) -> &str {
    if content.starts_with('\u{feff}') { &content[3..] } else { content }
}

// ===============================
// Default de Config
// ===============================

impl Default for Config {
    fn default() -> Self {
        Self {
            general: GeneralConfig::default(),
            logging: LoggingConfig::default(),
            log: LogConfig::default(),
            network: NetworkConfig::default(),
            wallet: WalletConfig::default(),
            executor: ExecutorConfig::default(),
            dex: vec![],
            pairs: PairsConfig::default(),
            addresses: HashMap::new(),
            flashloan: FlashloanConfig::default(),
            gas: GasConfig::default(),
            arbitrage: ArbitrageConfig::default(),
            execution: ExecutionConfig::default(),
            mev: MevConfig::default(),
            optimization: OptimizationConfig::default(),
            performance: PerformanceConfig::default(),
            prometheus: PrometheusConfig::default(),
            risk: RiskConfig::default(),
            telegram: None,
            monitoring: MonitoringConfig::default(),
            metrics: MetricsConfig::default(),
            database: DatabaseConfig::default(),
            explorer: ExplorerConfig::default(),
            api: ApiConfig::default(),
            emergency: EmergencyConfig::default(),
            strategy: StrategyConfig::default(),
            compliance: ComplianceConfig::default(),
            security: SecurityConfig::default(),
            backtesting: BacktestingConfig::default(),
            cache: CacheConfig::default(),
            rate_limiting: RateLimitingConfig::default(),
            price_feed: PriceFeedConfig::default(),
            detection: DetectionConfig::default(),
            radar: RadarConfig::default(),
            wrapper: WrapperConfig::default(),
            analytics: AnalyticsConfig::default(),
            validation: ValidationConfig::default(),
            alerts: AlertsConfig::default(),
            debug: DebugConfig::default(),
            summary: SummaryConfig::default(),
            min_profit_usd_threshold: 0.10,
            cooldown_manager: None,
        }
    }
}
#[cfg(test)]
mod config_parser_tests {
    use super::*;

    /// Sem `deny_unknown_fields`, chave com typo ou seção nunca implementada é
    /// aceita e descartada em silêncio. `ignored_toml_keys` usa o próprio serde
    /// (round-trip) para expor isso no boot.
    /// TOML completo e válido, a partir do default do próprio código.
    fn base_toml() -> String {
        toml::to_string(&Config::default()).expect("serialize default")
    }

    #[test]
    fn ignored_keys_detects_unknown_section() {
        let raw = format!(
            "{}\n[secao_que_ninguem_le]\nmax_hops = 3\nfoo = \"bar\"\n",
            base_toml()
        );
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let ignored = Config::ignored_toml_keys(&raw, &cfg);

        assert!(
            ignored.iter().any(|k| k == "secao_que_ninguem_le.max_hops"),
            "seção não implementada deve ser reportada, veio {ignored:?}"
        );
        assert!(ignored.iter().any(|k| k == "secao_que_ninguem_le.foo"));
    }

    #[test]
    fn ignored_keys_is_empty_for_fully_supported_toml() {
        // O default do código, por construção, só tem chaves que o parser lê.
        let raw = base_toml();
        let cfg: Config = toml::from_str(&raw).expect("parse");
        let ignored = Config::ignored_toml_keys(&raw, &cfg);
        assert!(ignored.is_empty(), "não deveria ignorar nada, veio {ignored:?}");
    }

    /// `[[dex]].liquidity_threshold_usd` era descartado pelo parser — o valor no
    /// TOML não chegava a lugar nenhum e todo venue caía no threshold global.
    #[test]
    fn per_dex_liquidity_threshold_survives_toml_roundtrip() {
        let mut cfg = Config::default();
        cfg.dex = vec![DexEntry {
            name: "UniswapV3".into(),
            router_address: "0x0000000000000000000000000000000000000001".into(),
            enabled: true,
            liquidity_threshold_usd: Some(10_000.0),
            ..Default::default()
        }];

        let raw = toml::to_string(&cfg).expect("serialize");
        assert!(
            raw.contains("liquidity_threshold_usd"),
            "campo deve existir na struct para ser serializado"
        );

        let back: Config = toml::from_str(&raw).expect("parse");
        assert_eq!(back.dex[0].liquidity_threshold_usd, Some(10_000.0));
        assert!(
            !Config::ignored_toml_keys(&raw, &back)
                .iter()
                .any(|k| k.contains("liquidity_threshold_usd")),
            "não pode mais ser ignorada"
        );
    }

    #[test]
    fn shipped_configs_have_no_ignored_keys() {
        for (name, raw) in [
            ("config.toml", include_str!("../../config/config.toml")),
            ("config.dryrun.toml", include_str!("../../config/config.dryrun.toml")),
            ("config.midtier.toml", include_str!("../../config/config.midtier.toml")),
        ] {
            let cfg: Config = toml::from_str(raw).expect(name);
            let ignored = Config::ignored_toml_keys(raw, &cfg);
            assert!(ignored.is_empty(), "{name} has ignored keys: {ignored:?}");
        }
    }
}
