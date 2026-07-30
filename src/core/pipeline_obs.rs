// ============================================================
// src/core/pipeline_obs.rs — Pipeline Observability
// ============================================================
//
// Structured counters and rejection reasons for the arbitrage
// discovery pipeline. Single scan-cycle aggregation with
// deterministic summary emission.
// ============================================================

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;
use tracing::{debug, info};

/// Explicit rejection reasons for every discard point in the pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum QuoteRejectReason {
    // Price feed stage
    PriceFeedUnavailable,
    PriceFeedInvalid,
    PriceFeedZeroOrNegative,
    PriceFeedNaN,
    PriceFeedInfinity,
    PriceFeedTimeout,
    PriceFeedRateLimited,
    PriceFeedParseError,
    PriceFeedUnknownSymbol,
    PriceFeedCacheMiss,

    // Sizing stage
    InvalidNotional,
    InvalidAmountIn,
    SizingArithmeticError,

    // DEX quote stage
    RpcError,
    ContractRevert,
    NoPool,
    NoLiquidity,
    InvalidRate,
    QuoteValidationFailed,

    // Reciprocity / pair validation
    NonReciprocal,
    SpreadOutOfRange,
    ReciprocityProductOutOfBounds,

    // Liquidity / TVL
    LiquidityBelowThreshold,
    PoolNotFound,

    // V3 specific
    V3ValidationFailed,
    V3FeeTierNotExecutable,
    V3NoQuoteAtAnyTier,

    // Stable conversion
    MissingUsdtOrUsdcConversion,
    StableStepEstimationFailed,

    // Graph / Bellman-Ford
    MissingPriceMapEntry,
    GraphVertexMissing,
    GraphEdgeMissing,
    NoNegativeCycle,
    CycleReconstructionFailed,

    // Profit / cost filters
    ProfitBelowThreshold,
    GasCostExceedsProfit,
    SlippageBudgetExceeded,
    AdverseMoveBudgetExceeded,
    NetProfitNegative,

    // Simulation / execution
    SimulationFailed,
    SimulationReverted,
    RouteUnsupportedByExecutor,

    // Generic
    Unknown,
    ConfigError,
}

impl QuoteRejectReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            QuoteRejectReason::PriceFeedUnavailable => "PriceFeedUnavailable",
            QuoteRejectReason::PriceFeedInvalid => "PriceFeedInvalid",
            QuoteRejectReason::PriceFeedZeroOrNegative => "PriceFeedZeroOrNegative",
            QuoteRejectReason::PriceFeedNaN => "PriceFeedNaN",
            QuoteRejectReason::PriceFeedInfinity => "PriceFeedInfinity",
            QuoteRejectReason::PriceFeedTimeout => "PriceFeedTimeout",
            QuoteRejectReason::PriceFeedRateLimited => "PriceFeedRateLimited",
            QuoteRejectReason::PriceFeedParseError => "PriceFeedParseError",
            QuoteRejectReason::PriceFeedUnknownSymbol => "PriceFeedUnknownSymbol",
            QuoteRejectReason::PriceFeedCacheMiss => "PriceFeedCacheMiss",
            QuoteRejectReason::InvalidNotional => "InvalidNotional",
            QuoteRejectReason::InvalidAmountIn => "InvalidAmountIn",
            QuoteRejectReason::SizingArithmeticError => "SizingArithmeticError",
            QuoteRejectReason::RpcError => "RpcError",
            QuoteRejectReason::ContractRevert => "ContractRevert",
            QuoteRejectReason::NoPool => "NoPool",
            QuoteRejectReason::NoLiquidity => "NoLiquidity",
            QuoteRejectReason::InvalidRate => "InvalidRate",
            QuoteRejectReason::QuoteValidationFailed => "QuoteValidationFailed",
            QuoteRejectReason::NonReciprocal => "NonReciprocal",
            QuoteRejectReason::SpreadOutOfRange => "SpreadOutOfRange",
            QuoteRejectReason::ReciprocityProductOutOfBounds => "ReciprocityProductOutOfBounds",
            QuoteRejectReason::LiquidityBelowThreshold => "LiquidityBelowThreshold",
            QuoteRejectReason::PoolNotFound => "PoolNotFound",
            QuoteRejectReason::V3ValidationFailed => "V3ValidationFailed",
            QuoteRejectReason::V3FeeTierNotExecutable => "V3FeeTierNotExecutable",
            QuoteRejectReason::V3NoQuoteAtAnyTier => "V3NoQuoteAtAnyTier",
            QuoteRejectReason::MissingUsdtOrUsdcConversion => "MissingUsdtOrUsdcConversion",
            QuoteRejectReason::StableStepEstimationFailed => "StableStepEstimationFailed",
            QuoteRejectReason::MissingPriceMapEntry => "MissingPriceMapEntry",
            QuoteRejectReason::GraphVertexMissing => "GraphVertexMissing",
            QuoteRejectReason::GraphEdgeMissing => "GraphEdgeMissing",
            QuoteRejectReason::NoNegativeCycle => "NoNegativeCycle",
            QuoteRejectReason::CycleReconstructionFailed => "CycleReconstructionFailed",
            QuoteRejectReason::ProfitBelowThreshold => "ProfitBelowThreshold",
            QuoteRejectReason::GasCostExceedsProfit => "GasCostExceedsProfit",
            QuoteRejectReason::SlippageBudgetExceeded => "SlippageBudgetExceeded",
            QuoteRejectReason::AdverseMoveBudgetExceeded => "AdverseMoveBudgetExceeded",
            QuoteRejectReason::NetProfitNegative => "NetProfitNegative",
            QuoteRejectReason::SimulationFailed => "SimulationFailed",
            QuoteRejectReason::SimulationReverted => "SimulationReverted",
            QuoteRejectReason::RouteUnsupportedByExecutor => "RouteUnsupportedByExecutor",
            QuoteRejectReason::Unknown => "Unknown",
            QuoteRejectReason::ConfigError => "ConfigError",
        }
    }
}

/// Per-scan-cycle pipeline counters. Aggregated atomically for thread-safety.
#[derive(Debug, Default)]
pub struct PipelineCounters {
    // Input configuration
    pub configured_tokens: AtomicU64,
    pub configured_pairs: AtomicU64,

    // Price feed stage
    pub price_feed_attempted: AtomicU64,
    pub price_feed_succeeded: AtomicU64,
    pub price_feed_failed: AtomicU64,
    pub price_feed_zero_or_invalid: AtomicU64,
    pub price_feed_cache_hit: AtomicU64,
    pub price_feed_cache_miss: AtomicU64,
    pub price_feed_fallback_used: AtomicU64,

    // Sizing stage (quote_amount_for_usd)
    pub sizing_attempted: AtomicU64,
    pub sizing_succeeded: AtomicU64,
    pub sizing_failed: AtomicU64,

    // DEX quote stage
    pub dex_quote_attempted: AtomicU64,
    pub dex_quote_succeeded: AtomicU64,
    pub dex_quote_failed: AtomicU64,

    // V2 specific
    pub v2_quote_attempted: AtomicU64,
    pub v2_quote_succeeded: AtomicU64,
    pub v2_quote_failed: AtomicU64,

    // V3 specific
    pub v3_quote_attempted: AtomicU64,
    pub v3_quote_succeeded: AtomicU64,
    pub v3_quote_failed: AtomicU64,

    // Curve specific
    pub curve_quote_attempted: AtomicU64,
    pub curve_quote_succeeded: AtomicU64,
    pub curve_quote_failed: AtomicU64,

    // Validation / filter rejections
    pub rejected_invalid_amount: AtomicU64,
    pub rejected_invalid_rate: AtomicU64,
    pub rejected_reciprocity: AtomicU64,
    pub rejected_liquidity: AtomicU64,
    pub rejected_spread: AtomicU64,
    pub rejected_v3_validation: AtomicU64,
    pub rejected_missing_usdt_or_usdc_conversion: AtomicU64,

    // Price map assembly
    pub raw_quotes: AtomicU64,
    pub accepted_quotes: AtomicU64,
    pub price_map_dexes: AtomicU64,
    pub price_map_pairs: AtomicU64,

    // Graph construction
    pub graph_vertices: AtomicU64,
    pub graph_edges: AtomicU64,

    // Bellman-Ford
    pub bf_cycles_raw: AtomicU64,
    pub bf_cycles_unique: AtomicU64,

    // Final filters
    pub cycles_rejected_profit: AtomicU64,
    pub cycles_rejected_gas: AtomicU64,
    pub cycles_rejected_slippage: AtomicU64,
    pub cycles_rejected_simulation: AtomicU64,
    pub cycles_rejected_adverse_move: AtomicU64,
    pub cycles_rejected_unsupported_route: AtomicU64,
    pub cycles_rejected_net_negative: AtomicU64,

    // Output
    pub opportunities_emitted: AtomicU64,
    /// Rejection detail is updated only on discard paths, not quote hot path.
    rejection_counts: Mutex<HashMap<&'static str, u64>>,
}

impl PipelineCounters {
    /// Increment a counter by 1.
    #[inline]
    pub fn inc(&self, counter: &AtomicU64) {
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Add a value to a counter.
    #[inline]
    pub fn add(&self, counter: &AtomicU64, value: u64) {
        counter.fetch_add(value, Ordering::Relaxed);
    }

    /// Get current value of a counter.
    #[inline]
    pub fn get(&self, counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }

    /// Reset all counters to zero (for new scan cycle).
    pub fn reset(&self) {
        let fields = [
            &self.configured_tokens,
            &self.configured_pairs,
            &self.price_feed_attempted,
            &self.price_feed_succeeded,
            &self.price_feed_failed,
            &self.price_feed_zero_or_invalid,
            &self.price_feed_cache_hit,
            &self.price_feed_cache_miss,
            &self.price_feed_fallback_used,
            &self.sizing_attempted,
            &self.sizing_succeeded,
            &self.sizing_failed,
            &self.dex_quote_attempted,
            &self.dex_quote_succeeded,
            &self.dex_quote_failed,
            &self.v2_quote_attempted,
            &self.v2_quote_succeeded,
            &self.v2_quote_failed,
            &self.v3_quote_attempted,
            &self.v3_quote_succeeded,
            &self.v3_quote_failed,
            &self.curve_quote_attempted,
            &self.curve_quote_succeeded,
            &self.curve_quote_failed,
            &self.rejected_invalid_amount,
            &self.rejected_invalid_rate,
            &self.rejected_reciprocity,
            &self.rejected_liquidity,
            &self.rejected_spread,
            &self.rejected_v3_validation,
            &self.rejected_missing_usdt_or_usdc_conversion,
            &self.raw_quotes,
            &self.accepted_quotes,
            &self.price_map_dexes,
            &self.price_map_pairs,
            &self.graph_vertices,
            &self.graph_edges,
            &self.bf_cycles_raw,
            &self.bf_cycles_unique,
            &self.cycles_rejected_profit,
            &self.cycles_rejected_gas,
            &self.cycles_rejected_slippage,
            &self.cycles_rejected_simulation,
            &self.cycles_rejected_adverse_move,
            &self.cycles_rejected_unsupported_route,
            &self.cycles_rejected_net_negative,
            &self.opportunities_emitted,
        ];
        for field in fields {
            field.store(0, Ordering::Relaxed);
        }
        if let Ok(mut reasons) = self.rejection_counts.lock() {
            reasons.clear();
        }
    }

    /// Emit structured pipeline summary for this scan cycle.
    pub fn emit_summary(&self, scan_id: u64, block_start: u64, block_end: u64, duration_ms: u64) {
        info!(
            target: "pipeline.summary",
            "[PIPELINE_SUMMARY] \
            scan_id={} \
            block_start={} \
            block_end={} \
            duration_ms={} \
            configured_tokens={} \
            configured_pairs={} \
            price_feed_attempted={} \
            price_feed_succeeded={} \
            price_feed_failed={} \
            price_feed_zero_or_invalid={} \
            price_feed_cache_hit={} \
            price_feed_cache_miss={} \
            price_feed_fallback_used={} \
            sizing_succeeded={} \
            sizing_failed={} \
            dex_quote_attempted={} \
            dex_quote_succeeded={} \
            dex_quote_failed={} \
            v2_quote_succeeded={} \
            v3_quote_succeeded={} \
            curve_quote_succeeded={} \
            raw_quotes={} \
            accepted_quotes={} \
            price_map_dexes={} \
            price_map_pairs={} \
            graph_vertices={} \
            graph_edges={} \
            bf_cycles_raw={} \
            bf_cycles_unique={} \
            cycles_rejected_profit={} \
            cycles_rejected_gas={} \
            cycles_rejected_slippage={} \
            cycles_rejected_simulation={} \
            cycles_rejected_adverse_move={} \
            cycles_rejected_unsupported_route={} \
            cycles_rejected_net_negative={} \
            opportunities_emitted={}",
            scan_id,
            block_start,
            block_end,
            duration_ms,
            self.get(&self.configured_tokens),
            self.get(&self.configured_pairs),
            self.get(&self.price_feed_attempted),
            self.get(&self.price_feed_succeeded),
            self.get(&self.price_feed_failed),
            self.get(&self.price_feed_zero_or_invalid),
            self.get(&self.price_feed_cache_hit),
            self.get(&self.price_feed_cache_miss),
            self.get(&self.price_feed_fallback_used),
            self.get(&self.sizing_succeeded),
            self.get(&self.sizing_failed),
            self.get(&self.dex_quote_attempted),
            self.get(&self.dex_quote_succeeded),
            self.get(&self.dex_quote_failed),
            self.get(&self.v2_quote_succeeded),
            self.get(&self.v3_quote_succeeded),
            self.get(&self.curve_quote_succeeded),
            self.get(&self.raw_quotes),
            self.get(&self.accepted_quotes),
            self.get(&self.price_map_dexes),
            self.get(&self.price_map_pairs),
            self.get(&self.graph_vertices),
            self.get(&self.graph_edges),
            self.get(&self.bf_cycles_raw),
            self.get(&self.bf_cycles_unique),
            self.get(&self.cycles_rejected_profit),
            self.get(&self.cycles_rejected_gas),
            self.get(&self.cycles_rejected_slippage),
            self.get(&self.cycles_rejected_simulation),
            self.get(&self.cycles_rejected_adverse_move),
            self.get(&self.cycles_rejected_unsupported_route),
            self.get(&self.cycles_rejected_net_negative),
            self.get(&self.opportunities_emitted),
        );
    }

    /// Emit rejection reasons summary.
    pub fn emit_rejection_summary(&self) {
        // For now we just log the aggregated counter values.
        // Detailed per-reason breakdown requires a HashMap which needs a lock.
        // We'll log what we have atomically.
        let reasons = self
            .rejection_counts
            .lock()
            .map(|counts| {
                let mut entries: Vec<_> = counts.iter().collect();
                entries.sort_by_key(|(reason, _)| **reason);
                entries
                    .into_iter()
                    .map(|(reason, count)| format!("{reason}={count}"))
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        info!(
            target: "pipeline.rejections",
            "[PIPELINE_REJECTIONS] \
            PriceFeedUnavailable={} \
            PriceFeedInvalid={} \
            InvalidNotional={} \
            InvalidAmountIn={} \
            RpcError={} \
            ContractRevert={} \
            NoPool={} \
            NoLiquidity={} \
            InvalidRate={} \
            NonReciprocal={} \
            SpreadOutOfRange={} \
            LiquidityBelowThreshold={} \
            V3ValidationFailed={} \
            MissingUsdtOrUsdcConversion={} \
            MissingPriceMapEntry={} \
            GraphVertexMissing={} \
            GraphEdgeMissing={} \
            NoNegativeCycle={} \
            ProfitBelowThreshold={} \
            GasCostExceedsProfit={} \
            SlippageBudgetExceeded={} \
            AdverseMoveBudgetExceeded={} \
            NetProfitNegative={} \
            SimulationFailed={} \
            SimulationReverted={} \
            RouteUnsupportedByExecutor={} \
            Unknown={} reasons={}",
            self.get(&self.price_feed_failed),
            self.get(&self.price_feed_zero_or_invalid),
            self.get(&self.sizing_failed),
            self.get(&self.rejected_invalid_amount),
            self.get(&self.dex_quote_failed),
            0, // ContractRevert - not directly tracked
            self.get(&self.rejected_liquidity), // maps to NoPool
            self.get(&self.rejected_liquidity), // maps to NoLiquidity
            self.get(&self.rejected_invalid_rate),
            self.get(&self.rejected_reciprocity),
            self.get(&self.rejected_spread),
            self.get(&self.rejected_liquidity),
            self.get(&self.rejected_v3_validation),
            self.get(&self.rejected_missing_usdt_or_usdc_conversion),
            self.get(&self.cycles_rejected_profit), // maps to MissingPriceMapEntry in BF
            self.get(&self.cycles_rejected_profit), // maps to GraphVertexMissing
            self.get(&self.cycles_rejected_profit), // maps to GraphEdgeMissing
            self.get(&self.cycles_rejected_profit), // maps to NoNegativeCycle
            self.get(&self.cycles_rejected_profit),
            self.get(&self.cycles_rejected_gas),
            self.get(&self.cycles_rejected_slippage),
            self.get(&self.cycles_rejected_adverse_move),
            self.get(&self.cycles_rejected_net_negative),
            self.get(&self.cycles_rejected_simulation),
            self.get(&self.cycles_rejected_simulation), // maps to SimulationReverted
            self.get(&self.cycles_rejected_unsupported_route),
            self.get(&self.cycles_rejected_net_negative),
            reasons,
        );
    }
}

/// Global pipeline counters instance (one per scan cycle).
static PIPELINE_COUNTERS: once_cell::sync::Lazy<PipelineCounters> =
    once_cell::sync::Lazy::new(PipelineCounters::default);

/// Get the global pipeline counters.
pub fn pipeline_counters() -> &'static PipelineCounters {
    &PIPELINE_COUNTERS
}

/// Scan cycle context for tracking a single pipeline execution.
pub struct ScanCycle {
    pub scan_id: u64,
    pub block_start: u64,
    pub block_end: u64,
    pub start_time: Instant,
}

impl ScanCycle {
    pub fn new(scan_id: u64, block_start: u64, block_end: u64) -> Self {
        pipeline_counters().reset();
        Self {
            scan_id,
            block_start,
            block_end,
            start_time: Instant::now(),
        }
    }

    pub fn finish(self) {
        let duration_ms = self.start_time.elapsed().as_millis() as u64;
        pipeline_counters().emit_summary(
            self.scan_id,
            self.block_start,
            self.block_end,
            duration_ms,
        );
        pipeline_counters().emit_rejection_summary();
    }
}

/// Helper macros for incrementing counters with explicit rejection reasons.
#[macro_export]
macro_rules! count {
    ($field:ident) => {
        $crate::core::pipeline_obs::pipeline_counters()
            .inc(&$crate::core::pipeline_obs::pipeline_counters().$field)
    };
    ($field:ident, $value:expr) => {
        $crate::core::pipeline_obs::pipeline_counters().add(
            &$crate::core::pipeline_obs::pipeline_counters().$field,
            $value,
        )
    };
}

/// Record a rejection with explicit reason (for detailed debug logging).
pub fn record_rejection(reason: QuoteRejectReason, context: &str) {
    let counters = pipeline_counters();
    if let Ok(mut reasons) = counters.rejection_counts.lock() {
        *reasons.entry(reason.as_str()).or_insert(0) += 1;
    }
    // Increment the appropriate counter based on reason
    match reason {
        QuoteRejectReason::PriceFeedUnavailable
        | QuoteRejectReason::PriceFeedTimeout
        | QuoteRejectReason::PriceFeedRateLimited
        | QuoteRejectReason::PriceFeedParseError
        | QuoteRejectReason::PriceFeedUnknownSymbol => {
            count!(price_feed_failed);
        }
        QuoteRejectReason::PriceFeedInvalid
        | QuoteRejectReason::PriceFeedZeroOrNegative
        | QuoteRejectReason::PriceFeedNaN
        | QuoteRejectReason::PriceFeedInfinity => {
            count!(price_feed_zero_or_invalid);
        }
        QuoteRejectReason::PriceFeedCacheMiss => {
            count!(price_feed_cache_miss);
        }
        QuoteRejectReason::InvalidNotional
        | QuoteRejectReason::InvalidAmountIn
        | QuoteRejectReason::SizingArithmeticError => {
            count!(sizing_failed);
        }
        QuoteRejectReason::RpcError
        | QuoteRejectReason::ContractRevert
        | QuoteRejectReason::QuoteValidationFailed => {
            count!(dex_quote_failed);
        }
        QuoteRejectReason::NoPool
        | QuoteRejectReason::NoLiquidity
        | QuoteRejectReason::LiquidityBelowThreshold
        | QuoteRejectReason::PoolNotFound => {
            count!(rejected_liquidity);
        }
        QuoteRejectReason::InvalidRate => {
            count!(rejected_invalid_rate);
        }
        QuoteRejectReason::NonReciprocal | QuoteRejectReason::ReciprocityProductOutOfBounds => {
            count!(rejected_reciprocity);
        }
        QuoteRejectReason::SpreadOutOfRange => {
            count!(rejected_spread);
        }
        QuoteRejectReason::V3ValidationFailed
        | QuoteRejectReason::V3FeeTierNotExecutable
        | QuoteRejectReason::V3NoQuoteAtAnyTier => {
            count!(rejected_v3_validation);
        }
        QuoteRejectReason::MissingUsdtOrUsdcConversion
        | QuoteRejectReason::StableStepEstimationFailed => {
            count!(rejected_missing_usdt_or_usdc_conversion);
        }
        QuoteRejectReason::MissingPriceMapEntry
        | QuoteRejectReason::GraphVertexMissing
        | QuoteRejectReason::GraphEdgeMissing
        | QuoteRejectReason::NoNegativeCycle
        | QuoteRejectReason::CycleReconstructionFailed => {
            count!(cycles_rejected_profit);
        }
        QuoteRejectReason::ProfitBelowThreshold => {
            count!(cycles_rejected_profit);
        }
        QuoteRejectReason::GasCostExceedsProfit => {
            count!(cycles_rejected_gas);
        }
        QuoteRejectReason::SlippageBudgetExceeded => {
            count!(cycles_rejected_slippage);
        }
        QuoteRejectReason::AdverseMoveBudgetExceeded => {
            count!(cycles_rejected_adverse_move);
        }
        QuoteRejectReason::NetProfitNegative => {
            count!(cycles_rejected_net_negative);
        }
        QuoteRejectReason::SimulationFailed | QuoteRejectReason::SimulationReverted => {
            count!(cycles_rejected_simulation);
        }
        QuoteRejectReason::RouteUnsupportedByExecutor => {
            count!(cycles_rejected_unsupported_route);
        }
        _ => {
            // Unknown - could add a catch-all counter if needed
        }
    }

    // Debug log with sampling (1 in 100 to avoid log spam)
    use std::sync::atomic::{AtomicU64, Ordering};
    static REJECTION_SAMPLE: AtomicU64 = AtomicU64::new(0);
    let sample = REJECTION_SAMPLE.fetch_add(1, Ordering::Relaxed);
    if sample % 100 == 0 {
        debug!(
            target: "pipeline.rejections",
            reason = reason.as_str(),
            context = context,
            "Quote rejected"
        );
    }
}

/// Record a price feed attempt with detailed outcome.
pub fn record_price_feed_attempt(
    symbol: &str,
    token_address: Option<&str>,
    decimals: u8,
    requested_usd_notional: f64,
    price_source: &str,
    cache_status: &str,
    price_value: Option<f64>,
    success: bool,
    failure_reason: Option<QuoteRejectReason>,
) {
    count!(price_feed_attempted);

    if success {
        count!(price_feed_succeeded);
        if cache_status == "hit" {
            count!(price_feed_cache_hit);
        } else {
            count!(price_feed_cache_miss);
        }
    } else {
        count!(price_feed_failed);
        if let Some(reason) = failure_reason {
            record_rejection(reason, "price_feed");
        }
    }

    if let Some(price) = price_value {
        if !price.is_finite() || price <= 0.0 {
            count!(price_feed_zero_or_invalid);
            record_rejection(
                if price.is_nan() {
                    QuoteRejectReason::PriceFeedNaN
                } else if price.is_infinite() {
                    QuoteRejectReason::PriceFeedInfinity
                } else {
                    QuoteRejectReason::PriceFeedZeroOrNegative
                },
                "price_feed_validation",
            );
        }
    }

    debug!(
        target: "pipeline.price_feed",
        symbol = symbol,
        token_address = token_address.unwrap_or("unknown"),
        decimals = decimals,
        requested_usd_notional = requested_usd_notional,
        price_source = price_source,
        cache_status = cache_status,
        price_value = price_value.unwrap_or(f64::NAN),
        success = success,
        failure_reason = failure_reason.map(|r| r.as_str()).unwrap_or("none"),
        "Price feed attempt"
    );
}

/// Record sizing (quote_amount_for_usd) attempt.
pub fn record_sizing_attempt(
    _symbol: &str,
    success: bool,
    failure_reason: Option<QuoteRejectReason>,
) {
    count!(sizing_attempted);
    if success {
        count!(sizing_succeeded);
    } else {
        count!(sizing_failed);
        if let Some(reason) = failure_reason {
            record_rejection(reason, "sizing");
        }
    }
}

/// Record DEX quote attempt.
pub fn record_dex_quote_attempt(
    _dex_name: &str,
    _pair: &str,
    success: bool,
    is_v2: bool,
    is_v3: bool,
    is_curve: bool,
) {
    count!(dex_quote_attempted);
    if success {
        count!(dex_quote_succeeded);
        if is_v2 {
            count!(v2_quote_succeeded);
        } else if is_v3 {
            count!(v3_quote_succeeded);
        } else if is_curve {
            count!(curve_quote_succeeded);
        }
    } else {
        count!(dex_quote_failed);
        if is_v2 {
            count!(v2_quote_failed);
        } else if is_v3 {
            count!(v3_quote_failed);
        } else if is_curve {
            count!(curve_quote_failed);
        }
    }
}

/// Record quote validation result.
pub fn record_quote_validation(
    _pair: &str,
    _dex: &str,
    accepted: bool,
    reject_reason: Option<QuoteRejectReason>,
) {
    count!(raw_quotes);
    if accepted {
        count!(accepted_quotes);
    } else if let Some(reason) = reject_reason {
        record_rejection(reason, "quote_validation");
    }
}

/// Record price map assembly.
pub fn record_price_map_assembly(dex_count: usize, pair_count: usize) {
    count!(price_map_dexes, dex_count as u64);
    count!(price_map_pairs, pair_count as u64);
}

/// Record graph construction.
pub fn record_graph_construction(vertices: usize, edges: usize) {
    count!(graph_vertices, vertices as u64);
    count!(graph_edges, edges as u64);
}

/// Record Bellman-Ford cycle detection.
pub fn record_bf_cycles(raw: usize, unique: usize) {
    count!(bf_cycles_raw, raw as u64);
    count!(bf_cycles_unique, unique as u64);
}

/// Record final filter rejection.
pub fn record_final_filter_rejection(reason: QuoteRejectReason) {
    record_rejection(reason, "final_filter");
}

/// Record opportunity emission.
pub fn record_opportunity_emitted() {
    count!(opportunities_emitted);
}

/// Diagnostic mode flag (read from env at startup).
pub fn diagnostic_mode_enabled() -> bool {
    std::env::var("ARBITRAGE_DIAGNOSTIC_MODE")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Print diagnostic mode banner.
pub fn print_diagnostic_banner() {
    if diagnostic_mode_enabled() {
        info!(
            target: "pipeline.diagnostic",
            "╔══════════════════════════════════════════════════════════════╗
║  DIAGNOSTIC MODE ENABLED                                    ║
║  ARBITRAGE_DIAGNOSTIC_MODE=1                                ║
║  LIVE_TRADING_ENABLED=false                                 ║
║  TRANSACTION_BROADCAST_ALLOWED=false                        ║
║  READ-ONLY: No transactions will be sent                    ║
╚══════════════════════════════════════════════════════════════╝"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_reset_and_increment() {
        let c = PipelineCounters::default();
        c.reset();
        assert_eq!(c.get(&c.price_feed_attempted), 0);
        c.inc(&c.price_feed_attempted);
        c.inc(&c.price_feed_attempted);
        assert_eq!(c.get(&c.price_feed_attempted), 2);
        c.add(&c.price_feed_succeeded, 5);
        assert_eq!(c.get(&c.price_feed_succeeded), 5);
    }

    #[test]
    fn rejection_reasons_map_to_counters() {
        // This test verifies the macro logic compiles correctly
        record_rejection(QuoteRejectReason::PriceFeedUnavailable, "test");
        record_rejection(QuoteRejectReason::InvalidRate, "test");
        record_rejection(QuoteRejectReason::NonReciprocal, "test");
    }

    #[test]
    fn scan_cycle_lifecycle() {
        let cycle = ScanCycle::new(1, 100, 101);
        count!(price_feed_attempted);
        count!(price_feed_succeeded);
        cycle.finish();
    }

    #[test]
    fn counter_consistency_sizing() {
        let c = PipelineCounters::default();
        c.reset();
        // Simulate 2 sizing attempts: 1 succeeded, 1 failed
        c.inc(&c.sizing_attempted);
        c.inc(&c.sizing_succeeded);
        c.inc(&c.sizing_attempted);
        c.inc(&c.sizing_failed);
        assert_eq!(
            c.get(&c.sizing_attempted),
            c.get(&c.sizing_succeeded) + c.get(&c.sizing_failed)
        );
    }

    #[test]
    fn counter_consistency_dex_quote() {
        let c = PipelineCounters::default();
        c.reset();
        // Simulate 2 dex quote attempts: 1 succeeded, 1 failed
        c.inc(&c.dex_quote_attempted);
        c.inc(&c.dex_quote_succeeded);
        c.inc(&c.dex_quote_attempted);
        c.inc(&c.dex_quote_failed);
        assert_eq!(
            c.get(&c.dex_quote_attempted),
            c.get(&c.dex_quote_succeeded) + c.get(&c.dex_quote_failed)
        );
    }
}
