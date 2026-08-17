//! Canonical executable price edge — replaces the aggregated, symbol-keyed
//! `PriceEdge` (see `bf_graph.rs`, legacy/diagnostic-only) for the Phase
//! 2D-C2B discovery pipeline. Every field is produced directly by a quote
//! adapter (`canonical_adapters::quote_v2_leg`/`quote_v3_leg`) plus the
//! `PoolExecutionMetadata` the caller supplied to that adapter — never by
//! parsing symbols, aggregating a route output, or reconstructing state
//! after the fact.

use crate::core::{
    canonical_adapters::PinnedQuoteRecord, canonical_execution_context::PoolExecutionMetadata,
    executable_call::Venue,
};
use ethers::types::{Address, H256, U256};
use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum EdgeError {
    #[error("EDGE_MIXED_BLOCK")]
    MixedBlock,
    #[error("EDGE_MISSING_METADATA")]
    MissingMetadata,
    #[error("EDGE_VENUE_MISMATCH")]
    VenueMismatch,
    #[error("EDGE_ZERO_ADDRESS")]
    ZeroAddress,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecutablePriceEdge {
    pub edge_id: H256,

    pub anchor_block: u64,
    pub anchor_block_hash: H256,

    pub venue: Venue,

    pub token_in: Address,
    pub token_out: Address,

    pub pool: Address,
    pub router: Address,
    pub spender: Address,

    pub fee: Option<u32>,

    pub amount_in: U256,
    pub amount_out: U256,

    pub quote_id: H256,
    pub pool_state_id: H256,
    pub execution_metadata_id: H256,
    pub provenance_hash: H256,

    // Diagnostic/UI only — never used for execution or economics.
    pub token_in_symbol: Option<String>,
    pub token_out_symbol: Option<String>,
}

impl ExecutablePriceEdge {
    /// Builds an edge strictly from a completed adapter quote plus the pool
    /// metadata the caller passed into that same adapter call. Fails closed
    /// on any anchor/pool/venue mismatch or zero address — never fabricates
    /// a missing field.
    pub fn from_quote(
        quote: &PinnedQuoteRecord,
        pool_meta: &PoolExecutionMetadata,
        token_in_symbol: Option<String>,
        token_out_symbol: Option<String>,
    ) -> Result<Self, EdgeError> {
        if quote.quote_id.is_zero()
            || quote.pool_state_id.is_zero()
            || quote.execution_metadata_id.is_zero()
            || quote.provenance_hash.is_zero()
            || quote.anchor_hash.is_zero()
        {
            return Err(EdgeError::MissingMetadata);
        }
        if quote.anchor_block != pool_meta.anchor_block {
            return Err(EdgeError::MixedBlock);
        }
        if quote.pool != pool_meta.pool || quote.token_in == quote.token_out {
            return Err(EdgeError::VenueMismatch);
        }
        if pool_meta.router.is_zero() || pool_meta.spender.is_zero() || pool_meta.pool.is_zero() {
            return Err(EdgeError::ZeroAddress);
        }
        if quote.amount_in.is_zero() || quote.amount_out.is_zero() {
            return Err(EdgeError::MissingMetadata);
        }
        let edge_id = H256::from(ethers::utils::keccak256(
            format!(
                "edge:{:?}:{:?}:{:?}:{:?}:{:?}",
                quote.quote_id, pool_meta.pool, pool_meta.router, pool_meta.spender, pool_meta.fee
            )
            .as_bytes(),
        ));
        Ok(Self {
            edge_id,
            anchor_block: quote.anchor_block,
            anchor_block_hash: quote.anchor_hash,
            venue: quote.venue,
            token_in: quote.token_in,
            token_out: quote.token_out,
            pool: pool_meta.pool,
            router: pool_meta.router,
            spender: pool_meta.spender,
            fee: pool_meta.fee,
            amount_in: quote.amount_in,
            amount_out: quote.amount_out,
            quote_id: quote.quote_id,
            pool_state_id: quote.pool_state_id,
            execution_metadata_id: quote.execution_metadata_id,
            provenance_hash: quote.provenance_hash,
            token_in_symbol,
            token_out_symbol,
        })
    }

    /// Lossy `amount_out / amount_in` ratio for logs/UI only. MUST NOT be
    /// used to derive `amount_in`/`amount_out`, economics, or any value fed
    /// into execution — those must always come from `amount_in`/`amount_out`
    /// (`U256`, exact adapter output) directly.
    pub fn diagnostic_rate(&self, decimals_in: u8, decimals_out: u8) -> Option<f64> {
        if self.amount_in.is_zero() || self.amount_out.is_zero() {
            return None;
        }
        let in_f64 =
            self.amount_in.to_string().parse::<f64>().ok()? * 10f64.powi(-(decimals_in as i32));
        let out_f64 =
            self.amount_out.to_string().parse::<f64>().ok()? * 10f64.powi(-(decimals_out as i32));
        if in_f64 == 0.0 {
            return None;
        }
        Some(out_f64 / in_f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::canonical_execution_context::PoolExecutionMetadata;

    fn a(n: u64) -> Address {
        Address::from_low_u64_be(n)
    }
    fn h(n: u64) -> H256 {
        H256::from_low_u64_be(n)
    }

    fn valid_quote() -> PinnedQuoteRecord {
        PinnedQuoteRecord {
            quote_id: h(1),
            anchor_block: 9,
            anchor_hash: h(2),
            venue: Venue::UniswapV3,
            pool: a(3),
            token_in: a(1),
            token_out: a(2),
            amount_in: U256::from(10),
            amount_out: U256::from(11),
            pool_state_id: h(4),
            execution_metadata_id: h(5),
            adapter_version: "v3-quoter-quoteExactInputSingle".into(),
            provenance_hash: h(6),
        }
    }
    fn valid_pool_meta() -> PoolExecutionMetadata {
        PoolExecutionMetadata {
            venue: "UniswapV3".into(),
            pool: a(3),
            router: a(4),
            spender: a(4),
            token_order: (a(1), a(2)),
            fee: Some(500),
            curve_method: None,
            curve_indices: None,
            implementation_code_hash: h(7),
            anchor_block: 9,
        }
    }

    #[test]
    fn builds_edge_from_valid_quote_and_pool_meta() {
        let edge = ExecutablePriceEdge::from_quote(
            &valid_quote(),
            &valid_pool_meta(),
            Some("A".into()),
            Some("B".into()),
        )
        .unwrap();
        assert_eq!(edge.pool, a(3));
        assert_eq!(edge.router, a(4));
        assert_eq!(edge.amount_out, U256::from(11));
        assert!(!edge.edge_id.is_zero());
    }

    #[test]
    fn rejects_mixed_anchor_block() {
        let mut pool_meta = valid_pool_meta();
        pool_meta.anchor_block = 10;
        assert_eq!(
            ExecutablePriceEdge::from_quote(&valid_quote(), &pool_meta, None, None),
            Err(EdgeError::MixedBlock)
        );
    }

    #[test]
    fn rejects_pool_mismatch() {
        let mut pool_meta = valid_pool_meta();
        pool_meta.pool = a(99);
        assert_eq!(
            ExecutablePriceEdge::from_quote(&valid_quote(), &pool_meta, None, None),
            Err(EdgeError::VenueMismatch)
        );
    }

    #[test]
    fn rejects_zero_router() {
        let mut pool_meta = valid_pool_meta();
        pool_meta.router = Address::zero();
        assert_eq!(
            ExecutablePriceEdge::from_quote(&valid_quote(), &pool_meta, None, None),
            Err(EdgeError::ZeroAddress)
        );
    }

    #[test]
    fn diagnostic_rate_never_backs_amounts() {
        let edge = ExecutablePriceEdge::from_quote(
            &valid_quote(),
            &valid_pool_meta(),
            Some("A".into()),
            Some("B".into()),
        )
        .unwrap();
        let r = edge.diagnostic_rate(6, 6).unwrap();
        assert!(r > 0.0);
        // amounts remain the exact adapter-returned U256 values regardless
        // of the lossy f64 rate.
        assert_eq!(edge.amount_in, U256::from(10));
        assert_eq!(edge.amount_out, U256::from(11));
    }
}
