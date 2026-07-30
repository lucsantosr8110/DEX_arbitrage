//! Guards and counters shared by the read-only Polygon diagnostic binary.
//! This module deliberately has no signer, wallet, executor, or broadcaster dependency.

use anyhow::{anyhow, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadOnlySafety {
    pub diagnostic_mode: bool,
    pub live_trading_enabled: bool,
    pub transaction_broadcast_allowed: bool,
}

impl ReadOnlySafety {
    pub fn from_env() -> Self {
        Self {
            diagnostic_mode: strict_true("ARBITRAGE_DIAGNOSTIC_MODE"),
            live_trading_enabled: strict_true("LIVE_TRADING_ENABLED"),
            transaction_broadcast_allowed: strict_true("TRANSACTION_BROADCAST_ALLOWED"),
        }
    }

    pub fn validate(self) -> Result<()> {
        if !self.diagnostic_mode {
            return Err(anyhow!(
                "ARBITRAGE_DIAGNOSTIC_MODE must be explicitly true or 1"
            ));
        }
        if self.live_trading_enabled {
            return Err(anyhow!(
                "LIVE_TRADING_ENABLED=true forbidden in read-only scan"
            ));
        }
        if self.transaction_broadcast_allowed {
            return Err(anyhow!(
                "TRANSACTION_BROADCAST_ALLOWED=true forbidden in read-only scan"
            ));
        }
        Ok(())
    }
}

pub fn strict_true(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("true") | Ok("1"))
}

#[derive(Debug, Default, Clone)]
pub struct ReadOnlyCounters {
    pub configured_tokens: u64,
    pub configured_pairs: u64,
    pub price_feed_attempted: u64,
    pub price_feed_succeeded_primary: u64,
    pub price_feed_succeeded_fallback: u64,
    pub price_feed_failed: u64,
    pub price_feed_invalid: u64,
    pub sizing_attempted: u64,
    pub sizing_succeeded: u64,
    pub sizing_failed: u64,
    pub dex_quote_attempted: u64,
    pub dex_quote_succeeded: u64,
    pub dex_quote_failed: u64,
    pub v2_quote_succeeded: u64,
    pub v3_quote_succeeded: u64,
    pub raw_quotes: u64,
    pub accepted_quotes: u64,
    pub rejected_reciprocity: u64,
    pub price_map_dexes: u64,
    pub price_map_pairs: u64,
    pub graph_vertices: u64,
    pub graph_edges: u64,
    pub bf_cycles_raw: u64,
    pub bf_cycles_unique: u64,
    pub opportunities_emitted: u64,
}

impl ReadOnlyCounters {
    pub fn valid(&self) -> bool {
        self.sizing_attempted == self.sizing_succeeded + self.sizing_failed
            && self.dex_quote_attempted == self.dex_quote_succeeded + self.dex_quote_failed
            && self.price_feed_attempted
                == self.price_feed_succeeded_primary
                    + self.price_feed_succeeded_fallback
                    + self.price_feed_failed
                    + self.price_feed_invalid
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_terminal_categories_do_not_duplicate() {
        let counters = ReadOnlyCounters {
            price_feed_attempted: 2,
            price_feed_succeeded_primary: 1,
            price_feed_failed: 1,
            sizing_attempted: 2,
            sizing_succeeded: 1,
            sizing_failed: 1,
            dex_quote_attempted: 2,
            dex_quote_succeeded: 1,
            dex_quote_failed: 1,
            ..Default::default()
        };
        assert!(counters.valid());
    }

    #[test]
    fn counter_duplicate_terminal_category_fails() {
        let counters = ReadOnlyCounters {
            sizing_attempted: 1,
            sizing_succeeded: 1,
            sizing_failed: 1,
            ..Default::default()
        };
        assert!(!counters.valid());
    }

    #[test]
    fn safety_rejects_broadcast_or_live_execution() {
        assert!(ReadOnlySafety {
            diagnostic_mode: true,
            live_trading_enabled: false,
            transaction_broadcast_allowed: true
        }
        .validate()
        .is_err());
        assert!(ReadOnlySafety {
            diagnostic_mode: true,
            live_trading_enabled: true,
            transaction_broadcast_allowed: false
        }
        .validate()
        .is_err());
    }

    #[test]
    fn safety_requires_explicit_diagnostic_mode() {
        assert!(ReadOnlySafety {
            diagnostic_mode: false,
            live_trading_enabled: false,
            transaction_broadcast_allowed: false
        }
        .validate()
        .is_err());
        assert!(ReadOnlySafety {
            diagnostic_mode: true,
            live_trading_enabled: false,
            transaction_broadcast_allowed: false
        }
        .validate()
        .is_ok());
    }
}
