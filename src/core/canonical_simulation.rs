//! Read-only canonical pending-state simulation.
//!
//! This deliberately accepts `Middleware`, not `SignerMiddleware`.  It builds
//! ordinary `eth_call` requests from the shared builder registry and never
//! exposes a method capable of signing or sending a transaction.

use crate::core::{
    executable_call::{ExecutableCallBuilderRegistry, ExecutionCallContext, Venue},
    executable_opportunity::ExecutableOpportunity,
    risk::RiskApproval,
    route_artifact::RouteLeg,
    types::BundleResult,
};
use anyhow::{anyhow, Result};
use ethers::{
    providers::Middleware,
    types::{Address, BlockId, BlockNumber, TransactionRequest, U256},
};
use std::sync::Arc;
use tokio::time::{timeout, Duration};

/// Non-overridable mode used by canonical code. It has no configuration
/// payload precisely so environment, CLI, or legacy TOML cannot enable send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanonicalExecutionMode {
    CanonicalDryRun,
}

impl CanonicalExecutionMode {
    pub const fn broadcast_allowed(self) -> bool {
        false
    }
    pub const fn signer_allowed(self) -> bool {
        false
    }
}

pub struct CanonicalSimulationClient<M> {
    provider: Arc<M>,
    from: Address,
    mode: CanonicalExecutionMode,
}

impl<M> CanonicalSimulationClient<M>
where
    M: Middleware,
    M::Error: 'static,
{
    pub fn new(provider: Arc<M>, from: Address) -> Self {
        Self {
            provider,
            from,
            mode: CanonicalExecutionMode::CanonicalDryRun,
        }
    }

    pub async fn simulate_pending(
        &self,
        opportunity: &ExecutableOpportunity,
        approval: &RiskApproval,
    ) -> Result<BundleResult> {
        debug_assert!(!self.mode.broadcast_allowed() && !self.mode.signer_allowed());
        if self.from.is_zero()
            || opportunity.amount_in.is_zero()
            || approval.min_profit_raw.is_zero()
        {
            return Err(anyhow!("CANONICAL_DRY_RUN_CALLER_OR_RISK_INVALID"));
        }
        let registry = ExecutableCallBuilderRegistry::standard();
        let mut amount = opportunity.amount_in;
        for typed in &opportunity.route_plan.legs {
            let venue = match typed.venue {
                Venue::QuickSwap => "QuickSwap",
                Venue::SushiSwap => "SushiSwap",
                Venue::UniswapV3 => "UniswapV3",
                Venue::Curve => return Err(anyhow!("CANONICAL_CURVE_FAIL_CLOSED")),
            };
            let leg = RouteLeg {
                // Derived from typed addresses only. Human route symbols never
                // enter the executable builder path.
                token_in: format!("{:#x}", typed.token_in),
                token_out: format!("{:#x}", typed.token_out),
                venue: venue.into(),
                protocol_version: if typed.venue == Venue::UniswapV3 {
                    "V3"
                } else {
                    "V2"
                }
                .into(),
                pool_address: Some(format!("{:#x}", typed.pool)),
                fee_tier: typed.fee,
            };
            let call = registry.build(
                &leg,
                amount,
                &ExecutionCallContext {
                    recipient: self.from,
                    deadline: U256::from(4_000_000_000u64),
                    amount_out_min: U256::zero(),
                    default_sqrt_price_limit_x96: U256::zero(),
                    router: Some(typed.router),
                    curve_method: None,
                    curve_indices: None,
                },
            )?;
            let request = TransactionRequest::new()
                .from(self.from)
                .to(call.target)
                .data(call.calldata)
                .value(call.value);
            timeout(
                Duration::from_secs(10),
                self.provider
                    .call(&request.into(), Some(BlockId::Number(BlockNumber::Pending))),
            )
            .await
            .map_err(|_| anyhow!("CANONICAL_PENDING_SIMULATION_TIMEOUT"))?
            .map_err(|_| anyhow!("CANONICAL_PENDING_SIMULATION_REVERT_OR_DECODE"))?;
            amount = opportunity
                .leg_quotes
                .iter()
                .find(|quote| quote.pool == typed.pool && quote.amount_in == amount)
                .map(|quote| quote.amount_out)
                .ok_or_else(|| anyhow!("CANONICAL_SEQUENTIAL_QUOTE_MISSING"))?;
        }
        Ok(BundleResult::new(true, 0.0, 0.0).with_execution_mode("canonical_dry_run_completed"))
    }
}
