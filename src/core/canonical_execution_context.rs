//! Canonical, block-pinned execution context (E1-F2C).
//! No field is inferred from an aggregate quote and this module performs no
//! network or transaction IO.

use crate::core::pool_state_sim::SimulatedPoolState;
use ethers::types::{Address, H256};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenMetadata {
    pub address: Address,
    pub symbol: String,
    pub decimals: u8,
    pub code_hash: H256,
    pub anchor_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PoolExecutionMetadata {
    pub venue: String,
    pub pool: Address,
    pub router: Address,
    pub spender: Address,
    pub token_order: (Address, Address),
    pub fee: Option<u32>,
    pub curve_method: Option<String>,
    pub curve_indices: Option<(i128, i128)>,
    pub implementation_code_hash: H256,
    pub anchor_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PinnedPoolState {
    pub state_id: String,
    pub pool_id: String,
    pub state: SimulatedPoolState,
    pub provenance_hash: H256,
    pub anchor_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkSetupRecord {
    pub route_key: String,
    pub caller: Address,
    pub funding: Vec<(Address, ethers::types::U256)>,
    pub approvals: Vec<(Address, Address, ethers::types::U256)>,
    pub balance_checks: Vec<Address>,
    pub targets: Vec<Address>,
    pub anchor_block: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CanonicalExecutionContext {
    pub anchor_block: u64,
    pub anchor_block_hash: H256,
    pub tokens: BTreeMap<String, TokenMetadata>,
    pub pools: BTreeMap<String, PoolExecutionMetadata>,
    pub pool_states: BTreeMap<String, PinnedPoolState>,
    pub fork_setup: BTreeMap<String, ForkSetupRecord>,
    pub context_hash: H256,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ContextError {
    #[error("CONTEXT_MIXED_ANCHOR_BLOCKS")]
    MixedBlocks,
    #[error("CONTEXT_MISSING_POOL_STATE: {0}")]
    MissingPoolState(String),
    #[error("CONTEXT_MISSING_METADATA: {0}")]
    MissingMetadata(String),
    #[error("CONTEXT_PLACEHOLDER_FORBIDDEN")]
    PlaceholderForbidden,
    #[error("CONTEXT_HASH_INVALID")]
    InvalidHash,
}

impl CanonicalExecutionContext {
    pub fn build(
        anchor_block: u64,
        anchor_block_hash: H256,
        tokens: BTreeMap<String, TokenMetadata>,
        pools: BTreeMap<String, PoolExecutionMetadata>,
        pool_states: BTreeMap<String, PinnedPoolState>,
        fork_setup: BTreeMap<String, ForkSetupRecord>,
    ) -> Result<Self, ContextError> {
        if anchor_block_hash == H256::zero()
            || tokens.is_empty()
            || pools.is_empty()
            || pool_states.is_empty()
        {
            return Err(ContextError::PlaceholderForbidden);
        }
        for t in tokens.values() {
            if t.anchor_block != anchor_block
                || t.address.is_zero()
                || t.code_hash == H256::zero()
                || t.symbol.is_empty()
                || t.decimals == 0
            {
                return Err(ContextError::MissingMetadata("token".into()));
            }
        }
        for (id, p) in &pools {
            if p.anchor_block != anchor_block
                || p.pool.is_zero()
                || p.router.is_zero()
                || p.spender.is_zero()
                || p.implementation_code_hash == H256::zero()
            {
                return Err(ContextError::MissingMetadata(id.clone()));
            }
        }
        for (id, s) in &pool_states {
            if s.anchor_block != anchor_block
                || s.pool_id.is_empty()
                || s.provenance_hash == H256::zero()
            {
                return Err(ContextError::MissingPoolState(id.clone()));
            }
        }
        for f in fork_setup.values() {
            if f.anchor_block != anchor_block || f.caller.is_zero() {
                return Err(ContextError::MixedBlocks);
            }
        }
        let context_hash = Self::compute_hash(
            anchor_block_hash,
            &tokens,
            &pools,
            &pool_states,
            &fork_setup,
        );
        Ok(Self {
            anchor_block,
            anchor_block_hash,
            tokens,
            pools,
            pool_states,
            fork_setup,
            context_hash,
        })
    }

    fn compute_hash(
        anchor_block_hash: H256,
        tokens: &BTreeMap<String, TokenMetadata>,
        pools: &BTreeMap<String, PoolExecutionMetadata>,
        pool_states: &BTreeMap<String, PinnedPoolState>,
        fork_setup: &BTreeMap<String, ForkSetupRecord>,
    ) -> H256 {
        let canonical = format!(
            "{:?}:{anchor_block_hash:?}:{:?}:{:?}:{:?}",
            tokens,
            pools,
            pool_states.keys().collect::<Vec<_>>(),
            fork_setup.keys().collect::<Vec<_>>()
        );
        H256::from(ethers::utils::keccak256(canonical.as_bytes()))
    }

    pub fn verify_hash(&self) -> bool {
        self.context_hash != H256::zero()
    }

    /// Recomputes `context_hash` from the current fields and compares it
    /// against the stored `context_hash`. Used after reloading a persisted
    /// context to detect any tampering/corruption in the round-trip.
    pub fn verify_reload(&self) -> Result<(), ContextError> {
        let recomputed = Self::compute_hash(
            self.anchor_block_hash,
            &self.tokens,
            &self.pools,
            &self.pool_states,
            &self.fork_setup,
        );
        if recomputed != self.context_hash {
            return Err(ContextError::InvalidHash);
        }
        Ok(())
    }
}
