//! E1-F4 real fork execution stages — concrete `OrchestratorStages` adapter.
//!
//! This file introduces no new decision logic: every judgment call
//! (economic positivity, ABI/calldata shape, trace anomaly, balance delta)
//! is made by the already-tested modules it wires together
//! (`fresh_economics`, `executable_call`, `executable_readonly`,
//! `fork_preflight`, `fork_route_executor`, `fork_trace_validation`,
//! `fork_balance_accounting`). It only performs the real RPC/IO that those
//! pure modules cannot perform themselves, and hands the ordering contract
//! to `c2b_orchestrator::execute_route` unchanged.
//!
//! `OrchestratorStages` is a synchronous trait; every method here bridges
//! into async RPC work via `block_on` (`tokio::task::block_in_place` +
//! `Handle::block_on`), which is the officially supported way to run
//! blocking-equivalent async work from inside a multi-thread `#[tokio::main]`
//! runtime without nesting a second runtime.

use crate::core::{
    c2b_orchestrator::{OrchestratorError, OrchestratorStages},
    executable_call::{ExecutableCall, ExecutableCallBuilderRegistry, ExecutionCallContext},
    executable_readonly::{
        AnvilExecutableReadOnlyVerifier, ExecutableReadOnlyRequest, ExecutableReadOnlyStatus,
        ExecutableReadOnlyVerifier,
    },
    fork_balance_accounting::gross_pnl_atomic,
    fork_preflight::{
        actual_output, propagate_output, validate_preflight_trace, PreflightLegResult,
        PreflightStatus,
    },
    fork_route_executor::{
        anvil_set_balance, anvil_set_storage_at, debug_trace_call_tracer, discover_balance_slot,
        erc20_balance_slot_key, send_and_wait,
    },
    fork_trace_validation::{TraceAnomaly, UNISWAP_V3_SWAP_CALLBACK_SELECTOR},
    fresh_economics::{
        FreshEconomicEvaluator, PinnedStateSnapshot, RouteSimulationResult, SimulationContext,
        StatefulRouteEvaluator,
    },
    pool_state_sim::SimulatedPoolState,
    route_artifact::{pool_identity, RouteLeg, StructuralRouteLeg},
};
use anyhow::{anyhow, Result as AnyResult};
use ethers::{
    abi::Abi,
    contract::Contract,
    providers::{Http, Middleware, Provider},
    types::{Address, BlockNumber, TransactionRequest, H256, U256},
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

const ERC20_ABI: &str = r#"[
  {"inputs":[{"name":"account","type":"address"}],"name":"balanceOf","outputs":[{"name":"","type":"uint256"}],"stateMutability":"view","type":"function"},
  {"inputs":[{"name":"spender","type":"address"},{"name":"amount","type":"uint256"}],"name":"approve","outputs":[{"name":"","type":"bool"}],"stateMutability":"nonpayable","type":"function"}
]"#;

fn block_on<F: std::future::Future>(fut: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
}

fn to_request(
    tx: ethers::types::transaction::eip2718::TypedTransaction,
    from: Address,
) -> TransactionRequest {
    let mut req = TransactionRequest::new().from(from);
    if let Some(to) = tx.to_addr() {
        req = req.to(*to);
    }
    if let Some(data) = tx.data() {
        req = req.data(data.clone());
    }
    req
}

/// Everything one physical route needs to be economically evaluated, built,
/// read-only verified, and fork-executed. `legs[i]` and `executable_legs[i]`
/// always describe the same leg (the string form is derived only from the
/// typed form — see R1's `STRING_LEGS_DERIVED_ONLY_FROM_EXECUTABLE_LEGS`).
#[derive(Debug, Clone)]
pub struct RoutePlan {
    pub legs: Vec<RouteLeg>,
    pub executable_legs: Vec<StructuralRouteLeg>,
    pub route_input: U256,
    pub anchor_block: u64,
    pub start_token_decimals: u8,
    pub pool_state_by_pool: HashMap<Address, SimulatedPoolState>,
    /// The real, already-fetched chained re-quote amounts (Phase B of the
    /// discovery round), in leg order — `first_touch_quotes[i]` is leg i's
    /// real quoted `amount_out`, which is also leg `i+1`'s planned
    /// `amount_in`.
    pub first_touch_quotes: Vec<U256>,
}

#[derive(Debug, Clone, Default)]
pub struct RouteExecutionRecord {
    pub economics: Option<RouteSimulationResult>,
    pub readonly_results: Vec<crate::core::executable_readonly::ExecutableReadOnlyResult>,
    pub preflight_results: Vec<PreflightLegResult>,
    pub trace_anomalies: Vec<(usize, H256, Vec<TraceAnomaly>)>,
    pub tx_hashes: Vec<H256>,
    pub gas_used_total: u64,
    pub gross_pnl_atomic: Option<i128>,
    pub loss_on_fork: bool,
    pub error: Option<String>,
}

struct PreflightOutcome {
    balance_delta: U256,
    propagated: bool,
    preflight_results: Vec<PreflightLegResult>,
    tx_hashes: Vec<H256>,
    gas_used_total: u64,
    gross_pnl_atomic: Option<i128>,
    loss_on_fork: bool,
}

/// Real, loopback-only Anvil adapter for `OrchestratorStages`. One instance
/// is built per (round, anchor_block); the caller must `anvil_reset_to_block`
/// back to that anchor before every route so `readonly_eth_call`'s
/// anchor-block check holds and routes never see each other's mutated state.
pub struct RealForkStages {
    provider: Arc<Provider<Http>>,
    fork_endpoint: String,
    caller: Address,
    routes: HashMap<String, RoutePlan>,
    working_calls: Vec<ExecutableCall>,
    pub evidence: HashMap<String, RouteExecutionRecord>,
}

impl RealForkStages {
    pub fn new(
        provider: Arc<Provider<Http>>,
        fork_endpoint: String,
        caller: Address,
        routes: HashMap<String, RoutePlan>,
    ) -> Self {
        Self {
            provider,
            fork_endpoint,
            caller,
            routes,
            working_calls: Vec::new(),
            evidence: HashMap::new(),
        }
    }

    fn deadline(&self, plan: &RoutePlan) -> U256 {
        let _ = plan;
        block_on(async {
            self.provider
                .get_block(BlockNumber::Latest)
                .await
                .ok()
                .flatten()
                .and_then(|b| b.timestamp.checked_add(U256::from(3600u64)))
                .unwrap_or_else(|| U256::from(9_999_999_999u64))
        })
    }
}

impl OrchestratorStages for RealForkStages {
    fn stateful_economics(&mut self, key: &str) -> Result<bool, OrchestratorError> {
        let Some(plan) = self.routes.get(key) else {
            return Err(OrchestratorError::Stage("route_plan_missing"));
        };
        let mut pools: HashMap<String, SimulatedPoolState> = HashMap::new();
        for (leg, exec_leg) in plan.legs.iter().zip(plan.executable_legs.iter()) {
            if let Some(state) = plan.pool_state_by_pool.get(&exec_leg.pool) {
                pools.insert(pool_identity(leg), *state);
            }
        }
        let snapshot = PinnedStateSnapshot { pools };
        let context = SimulationContext {
            start_decimals: plan.start_token_decimals,
            // Gas is paid in native POL, a different unit than the
            // start-token atomic PnL computed here; this gate checks gross
            // on-chain-math profitability only. Real gas is measured and
            // reported separately once the preflight stage has real
            // receipts.
            gas_cost_atomic: U256::zero(),
        };
        let record = self.evidence.entry(key.to_string()).or_default();
        match StatefulRouteEvaluator.evaluate(
            &plan.legs,
            plan.route_input,
            &snapshot,
            &context,
            &plan.first_touch_quotes,
        ) {
            Ok(result) => {
                let positive = result.net_pnl_atomic > 0;
                record.economics = Some(result);
                Ok(positive)
            }
            Err(e) => {
                record.error = Some(format!("ECONOMICS_FAILED: {e}"));
                Ok(false)
            }
        }
    }

    fn build_executable_call(
        &mut self,
        key: &str,
        amount_in: U256,
    ) -> Result<U256, OrchestratorError> {
        let Some(plan) = self.routes.get(key).cloned() else {
            return Err(OrchestratorError::Stage("route_plan_missing"));
        };
        let deadline = self.deadline(&plan);
        self.working_calls.clear();
        let registry = ExecutableCallBuilderRegistry::standard();
        let mut current_amount = plan.route_input;
        for (i, exec_leg) in plan.executable_legs.iter().enumerate() {
            let ctx = ExecutionCallContext {
                recipient: self.caller,
                deadline,
                amount_out_min: U256::zero(),
                default_sqrt_price_limit_x96: U256::zero(),
                router: Some(exec_leg.router),
                curve_method: None,
                curve_indices: None,
            };
            // `plan.legs[i]` is the human-readable string form keyed by
            // *symbol* (`"USDC"`), kept only for `structural_cycle_key`
            // formatting — never a valid `Address`. The builders need real
            // on-chain addresses, which only the typed `exec_leg` (the same
            // leg, by construction — see `RoutePlan::legs` doc comment)
            // carries. Substituting them here, rather than changing what
            // `plan.legs[i]` means, keeps the string form's one documented
            // purpose intact.
            let mut typed_leg = plan.legs[i].clone();
            typed_leg.token_in = format!("{:?}", exec_leg.token_in);
            typed_leg.token_out = format!("{:?}", exec_leg.token_out);
            let call = registry
                .build(&typed_leg, current_amount, &ctx)
                .map_err(|e| {
                    let record = self.evidence.entry(key.to_string()).or_default();
                    record.error = Some(format!("BUILD_FAILED leg={i}: {e}"));
                    OrchestratorError::Stage("build_executable_call")
                })?;
            self.working_calls.push(call);
            current_amount = plan
                .first_touch_quotes
                .get(i)
                .copied()
                .unwrap_or(current_amount);
        }
        if self.working_calls.len() != plan.executable_legs.len() {
            return Err(OrchestratorError::Stage("build_executable_call_incomplete"));
        }
        Ok(amount_in)
    }

    fn readonly_eth_call(&mut self, key: &str, amount_in: U256) -> Result<U256, OrchestratorError> {
        let Some(plan) = self.routes.get(key).cloned() else {
            return Err(OrchestratorError::Stage("route_plan_missing"));
        };
        let verifier = match AnvilExecutableReadOnlyVerifier::new(
            self.fork_endpoint.clone(),
            self.provider.clone(),
        ) {
            Ok(v) => v,
            Err(_) => return Ok(U256::zero()),
        };
        let calls = self.working_calls.clone();
        let caller = self.caller;
        let route_key = key.to_string();
        let results = block_on(async {
            let mut out = Vec::new();
            let mut planned_amount_in = plan.route_input;
            for (i, call) in calls.into_iter().enumerate() {
                let request = ExecutableReadOnlyRequest {
                    route_key: route_key.clone(),
                    leg_index: i,
                    anchor_block: plan.anchor_block,
                    caller,
                    call,
                    expected_amount_in: planned_amount_in,
                    reset_before_call: false,
                };
                let result = verifier.verify(&request).await;
                planned_amount_in = plan
                    .first_touch_quotes
                    .get(i)
                    .copied()
                    .unwrap_or(planned_amount_in);
                out.push(result);
            }
            out
        });
        let mut all_pass = !results.is_empty();
        let mut last_amount_out = U256::zero();
        let mut readonly_results = Vec::new();
        for result in results {
            match result {
                Ok(r) => {
                    if r.status != ExecutableReadOnlyStatus::Pass {
                        all_pass = false;
                    }
                    last_amount_out = r.decoded_amount_out.unwrap_or_default();
                    readonly_results.push(r);
                }
                Err(_) => {
                    all_pass = false;
                }
            }
        }
        self.evidence
            .entry(key.to_string())
            .or_default()
            .readonly_results = readonly_results;
        let _ = amount_in;
        if !all_pass || last_amount_out.is_zero() {
            return Ok(U256::zero());
        }
        Ok(last_amount_out)
    }

    fn anvil_preflight(
        &mut self,
        key: &str,
        amount_in: U256,
    ) -> Result<(U256, bool), OrchestratorError> {
        let _ = amount_in;
        let Some(plan) = self.routes.get(key).cloned() else {
            return Err(OrchestratorError::Stage("route_plan_missing"));
        };
        let provider = self.provider.clone();
        let caller = self.caller;
        let outcome = block_on(run_preflight(provider, caller, plan));
        match outcome {
            Ok(o) => {
                let record = self.evidence.entry(key.to_string()).or_default();
                record.preflight_results = o.preflight_results;
                record.tx_hashes = o.tx_hashes;
                record.gas_used_total = o.gas_used_total;
                record.gross_pnl_atomic = o.gross_pnl_atomic;
                record.loss_on_fork = o.loss_on_fork;
                Ok((o.balance_delta, o.propagated))
            }
            Err(e) => {
                self.evidence.entry(key.to_string()).or_default().error =
                    Some(format!("PREFLIGHT_FAILED: {e}"));
                Ok((U256::zero(), false))
            }
        }
    }

    fn validate_trace(&mut self, key: &str) -> Result<bool, OrchestratorError> {
        let Some(plan) = self.routes.get(key) else {
            return Ok(false);
        };
        let hashes = self
            .evidence
            .get(key)
            .map(|r| r.tx_hashes.clone())
            .unwrap_or_default();
        if hashes.is_empty() {
            return Ok(false);
        }
        let mut allowlist: HashSet<Address> = HashSet::new();
        let mut has_v3_leg = false;
        for exec_leg in &plan.executable_legs {
            allowlist.insert(exec_leg.router);
            allowlist.insert(exec_leg.pool);
            allowlist.insert(exec_leg.token_in);
            allowlist.insert(exec_leg.token_out);
            has_v3_leg |= exec_leg.fee.is_some();
        }
        let mut callbacks: HashSet<[u8; 4]> = HashSet::new();
        if has_v3_leg {
            callbacks.insert(UNISWAP_V3_SWAP_CALLBACK_SELECTOR);
        }
        let provider = self.provider.clone();
        let traces = block_on(async {
            let mut out = Vec::new();
            for h in &hashes {
                out.push((*h, debug_trace_call_tracer(&provider, *h).await));
            }
            out
        });
        let mut all_clean = true;
        let mut anomaly_records = Vec::new();
        for (i, (hash, trace_result)) in traces.into_iter().enumerate() {
            match trace_result {
                Ok(trace) => {
                    let anomalies = validate_preflight_trace(&trace, &allowlist, &callbacks);
                    if !anomalies.is_empty() {
                        all_clean = false;
                    }
                    anomaly_records.push((i, hash, anomalies));
                }
                Err(_) => {
                    all_clean = false;
                    anomaly_records.push((i, hash, Vec::new()));
                }
            }
        }
        if let Some(record) = self.evidence.get_mut(key) {
            record.trace_anomalies = anomaly_records;
        }
        Ok(all_clean)
    }
}

async fn run_preflight(
    provider: Arc<Provider<Http>>,
    caller: Address,
    plan: RoutePlan,
) -> AnyResult<PreflightOutcome> {
    let erc20_abi: Abi = serde_json::from_str(ERC20_ABI)?;
    let start_token = plan
        .executable_legs
        .first()
        .ok_or_else(|| anyhow!("EMPTY_ROUTE"))?
        .token_in;
    let start_contract = Contract::new(start_token, erc20_abi.clone(), provider.clone());

    // Fund the caller with native gas + start-token balance via an
    // empirically-discovered storage override — fork-only state, never a
    // real transfer, never touches mainnet.
    anvil_set_balance(&provider, caller, U256::exp10(20)).await?;
    let balance_slot = discover_balance_slot(provider.clone(), start_token, caller, 30).await?;
    let slot_key = erc20_balance_slot_key(caller, balance_slot);
    let mut amount_bytes = [0u8; 32];
    plan.route_input.to_big_endian(&mut amount_bytes);
    anvil_set_storage_at(&provider, start_token, slot_key, H256::from(amount_bytes)).await?;

    let funded_balance: U256 = start_contract
        .method::<_, U256>("balanceOf", caller)?
        .call()
        .await?;
    if funded_balance != plan.route_input {
        return Err(anyhow!(
            "FUNDING_BALANCE_VERIFICATION_FAILED expected={} actual={}",
            plan.route_input,
            funded_balance
        ));
    }

    let deadline = U256::from(
        provider
            .get_block(BlockNumber::Latest)
            .await?
            .and_then(|b| b.timestamp.checked_add(U256::from(3600u64)))
            .unwrap_or_else(|| U256::from(9_999_999_999u64))
            .as_u64(),
    );

    let registry = ExecutableCallBuilderRegistry::standard();
    let mut current_amount = plan.route_input;
    let mut preflight_results = Vec::new();
    let mut tx_hashes = Vec::new();
    let mut gas_used_total: u64 = 0;
    let mut all_propagated = true;

    for (i, exec_leg) in plan.executable_legs.iter().enumerate() {
        let ctx = ExecutionCallContext {
            recipient: caller,
            deadline,
            amount_out_min: U256::zero(),
            default_sqrt_price_limit_x96: U256::zero(),
            router: Some(exec_leg.router),
            curve_method: None,
            curve_indices: None,
        };
        let call = registry
            .build(&plan.legs[i], current_amount, &ctx)
            .map_err(|e| anyhow!("BUILD_FAILED leg={i}: {e}"))?;

        for approval in &call.approvals {
            let token_contract = Contract::new(approval.token, erc20_abi.clone(), provider.clone());
            let approve_tx = token_contract
                .method::<_, bool>("approve", (approval.spender, approval.amount))?
                .tx;
            let approve_receipt = send_and_wait(&provider, to_request(approve_tx, caller)).await?;
            if approve_receipt.status.map(|s| s.as_u64()) != Some(1) {
                return Err(anyhow!("APPROVAL_REVERTED leg={i}"));
            }
        }

        let out_contract = Contract::new(exec_leg.token_out, erc20_abi.clone(), provider.clone());
        let balance_before: U256 = out_contract
            .method::<_, U256>("balanceOf", caller)?
            .call()
            .await?;

        let swap_request = TransactionRequest {
            from: Some(caller),
            to: Some(call.target.into()),
            data: Some(call.calldata.clone()),
            value: Some(call.value),
            ..Default::default()
        };
        let receipt = send_and_wait(&provider, swap_request).await?;
        let gas_used = receipt.gas_used.unwrap_or_default().as_u64();
        gas_used_total = gas_used_total.saturating_add(gas_used);
        tx_hashes.push(receipt.transaction_hash);

        let balance_after: U256 = out_contract
            .method::<_, U256>("balanceOf", caller)?
            .call()
            .await?;
        let status = if receipt.status.map(|s| s.as_u64()) == Some(1) {
            PreflightStatus::Pass
        } else {
            PreflightStatus::Revert
        };
        let actual = if status == PreflightStatus::Pass {
            actual_output(balance_before, balance_after).ok()
        } else {
            None
        };

        preflight_results.push(PreflightLegResult {
            leg_index: i,
            target: call.target,
            selector: call.selector,
            receipt_status: receipt.status.map(|s| s.as_u64()),
            gas_used: Some(gas_used),
            balance_before,
            balance_after,
            actual_amount_out: actual.unwrap_or_default(),
            status,
            revert_code: if status == PreflightStatus::Pass {
                None
            } else {
                Some("SWAP_REVERTED".to_string())
            },
        });

        let Some(actual_amount) = actual.filter(|a| !a.is_zero()) else {
            all_propagated = false;
            break;
        };
        if propagate_output(actual_amount, actual_amount).is_err() {
            all_propagated = false;
            break;
        }
        current_amount = actual_amount;
    }

    let all_pass = preflight_results.len() == plan.executable_legs.len()
        && preflight_results
            .iter()
            .all(|r| r.status == PreflightStatus::Pass);

    let final_balance: U256 = start_contract
        .method::<_, U256>("balanceOf", caller)?
        .call()
        .await?;
    let gross = gross_pnl_atomic(plan.route_input, final_balance).ok();
    let loss_on_fork = gross.map(|g| g <= 0).unwrap_or(true);

    let balance_delta = if all_pass && !loss_on_fork {
        actual_output(plan.route_input, final_balance).unwrap_or_default()
    } else {
        U256::zero()
    };
    let propagated = all_pass && all_propagated && !loss_on_fork;
    let _ = current_amount;

    Ok(PreflightOutcome {
        balance_delta,
        propagated,
        preflight_results,
        tx_hashes,
        gas_used_total,
        gross_pnl_atomic: gross,
        loss_on_fork,
    })
}
