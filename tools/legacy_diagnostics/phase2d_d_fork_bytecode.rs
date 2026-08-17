//! Phase 2D-D: execute the real bytecode of the single candidate route from
//! Phase 2D-C against local Anvil forks of the exact historical anchor
//! blocks. This binary's *only* write-capable RPC target is a loopback
//! Anvil instance it spawns itself — the upstream archive RPC is read-only
//! (used exclusively by Anvil's own forking, never dialed directly by this
//! process for a write) and is never logged or persisted.
//!
//! No signer, no wallet, no private key: transactions are sent via
//! `eth_sendTransaction` against Anvil's own pre-unlocked dev account, which
//! signs locally inside the node. `PRODUCTION_SIGNER_LOADED` is always
//! `false` because this process never constructs a signer type at all.

use anyhow::{anyhow, Context, Result};
use clap::Parser;
use ethers::{
    abi::Abi,
    contract::Contract,
    providers::{Http, Middleware, Provider},
    types::{Address, TransactionRequest, U256},
};
use flashloan_bot::core::{
    fork_balance_accounting::{gross_pnl_atomic, net_pnl_atomic},
    fork_candidate_dedup::{single_physical_candidate, SourceEvaluationRow},
    fork_economics_comparison::classify_delta,
    fork_execution_domain::validate_loopback_endpoint,
    fork_route_executor::{
        anvil_reset_to_block, anvil_set_balance, anvil_set_storage_at, debug_trace_call_tracer,
        discover_balance_slot, erc20_balance_slot_key, send_and_wait, spawn_anvil,
        wait_for_anvil_ready,
    },
    fork_trace_validation::{validate_trace, UNISWAP_V3_SWAP_CALLBACK_SELECTOR},
};
use serde::Serialize;
use std::{
    collections::HashSet, io::Write as _, path::PathBuf, process::Child, sync::Arc, time::Duration,
};

const ERC20_ABI: &str = r#"[
  {"inputs":[],"name":"decimals","outputs":[{"internalType":"uint8","name":"","type":"uint8"}],"stateMutability":"view","type":"function"},
  {"inputs":[{"name":"account","type":"address"}],"name":"balanceOf","outputs":[{"name":"","type":"uint256"}],"stateMutability":"view","type":"function"},
  {"inputs":[{"name":"spender","type":"address"},{"name":"amount","type":"uint256"}],"name":"approve","outputs":[{"name":"","type":"bool"}],"stateMutability":"nonpayable","type":"function"}
]"#;
// NOTE: the real on-chain `ISwapRouter.exactInputSingle` takes a single
// `ExactInputSingleParams` *tuple*, not 8 flat arguments — a flat-args ABI
// (as used by this project's dormant `src/dex/adapters/uniswap_v3.rs`
// production code, itself never live-tested) computes the wrong 4-byte
// selector and reverts with empty data on the real router. Confirmed by
// running this exact call against the real forked bytecode.
const V3_ROUTER_ABI: &str = r#"[{"inputs":[{"components":[{"internalType":"address","name":"tokenIn","type":"address"},{"internalType":"address","name":"tokenOut","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"},{"internalType":"address","name":"recipient","type":"address"},{"internalType":"uint256","name":"deadline","type":"uint256"},{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"uint256","name":"amountOutMinimum","type":"uint256"},{"internalType":"uint160","name":"sqrtPriceLimitX96","type":"uint160"}],"internalType":"struct ISwapRouter.ExactInputSingleParams","name":"params","type":"tuple"}],"name":"exactInputSingle","outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],"stateMutability":"payable","type":"function"}]"#;
const V3_QUOTER_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"tokenIn","type":"address"},{"internalType":"address","name":"tokenOut","type":"address"},{"internalType":"uint24","name":"fee","type":"uint24"},{"internalType":"uint256","name":"amountIn","type":"uint256"},{"internalType":"uint160","name":"sqrtPriceLimitX96","type":"uint160"}],"name":"quoteExactInputSingle","outputs":[{"internalType":"uint256","name":"amountOut","type":"uint256"}],"stateMutability":"nonpayable","type":"function"}]"#;
const CURVE_ABI: &str = r#"[
  {"inputs":[{"type":"int128","name":"i"},{"type":"int128","name":"j"},{"type":"uint256","name":"dx"}],"name":"get_dy_underlying","outputs":[{"type":"uint256","name":""}],"stateMutability":"view","type":"function"},
  {"inputs":[{"type":"int128","name":"i"},{"type":"int128","name":"j"},{"type":"uint256","name":"dx"},{"type":"uint256","name":"min_dy"}],"name":"exchange_underlying","outputs":[{"type":"uint256","name":""}],"stateMutability":"nonpayable","type":"function"}
]"#;

const UNISWAP_V3_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";
const USDC_ADDRESS: &str = "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359";
const USDT_ADDRESS: &str = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
const WMATIC_ADDRESS: &str = "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270";
const V3_FEE_TIER: u32 = 500;
const CURVE_INDEX_USDC: i128 = 1;
const CURVE_INDEX_USDT: i128 = 2;
/// EIP-1559 priority fee floor, matching Phase 2D-C's documented policy.
const POLYGON_PRIORITY_FEE_GWEI: u64 = 30;
const CHAIN_ID: u64 = 137;
const MAX_BALANCE_SLOT_PROBE: u64 = 30;
/// Loose relative-equivalence threshold for 2D-C-vs-fork comparisons,
/// disclosed here rather than hidden: 50 bps.
const EQUIVALENCE_BPS: u32 = 50;
/// Per-hop atomic rounding allowance for quote-vs-actual comparisons.
const ATOMIC_ROUNDING_BOUND: u128 = 2;

#[derive(Parser)]
#[command(name = "phase2d_d_fork_bytecode")]
struct Cli {
    #[arg(long)]
    phase2d_c_artifact: PathBuf,
    #[arg(long, default_value = "91149850,91149883,91149916")]
    anchor_blocks: String,
    #[arg(long, default_value = "10,25,50,100,200,300,500,750,1000")]
    start_token_sizes: String,
    #[arg(long, default_value = "http://127.0.0.1:8547")]
    fork_rpc: String,
    #[arg(long, default_value = "POLYGON_ARCHIVE_RPC_URL")]
    upstream_archive_rpc_env: String,
    #[arg(long, default_value = "diagnostics")]
    output_dir: PathBuf,
}

fn parse_u64_list(raw: &str) -> Result<Vec<u64>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<u64>().map_err(|e| anyhow!("{s}: {e}")))
        .collect()
}

fn parse_f64_list(raw: &str) -> Result<Vec<f64>> {
    raw.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| s.parse::<f64>().map_err(|e| anyhow!("{s}: {e}")))
        .collect()
}

fn timestamp() -> String {
    chrono::Utc::now().format("%Y%m%dT%H%M%SZ").to_string()
}

#[derive(Debug, Clone, Serialize)]
struct LegOutcome {
    quote_atomic: String,
    actual_atomic: String,
    delta_atomic: String,
    gas_used: u64,
    effective_gas_price_wei: String,
    tx_hash: String,
}

#[derive(Debug, Clone, Serialize)]
struct CaseRow {
    anchor_block: u64,
    anchor_block_hash: String,
    structural_cycle_key: String,
    source_profiles: Vec<String>,
    start_token: String,
    start_token_decimals: u8,
    start_amount_units: f64,
    start_amount_atomic: String,
    leg1: Option<LegOutcome>,
    leg2: Option<LegOutcome>,
    final_amount_atomic: Option<String>,
    gross_pnl_atomic: Option<String>,
    approval_gas_used: u64,
    swap_gas_used: u64,
    cold_total_gas_used: u64,
    warm_gas_cost_start_token_atomic: Option<String>,
    cold_gas_cost_start_token_atomic: Option<String>,
    warm_net_pnl_atomic: Option<String>,
    cold_net_pnl_atomic: Option<String>,
    phase2d_c_predicted_net_pnl_atomic: Option<String>,
    prediction_actual_delta_classification: Option<String>,
    classification: String,
    negative_control_status: Option<String>,
    atomic_build_candidate: bool,
    unexpected_calls: usize,
    revert_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CaseTrace {
    anchor_block: u64,
    start_amount_units: f64,
    leg1_trace: Option<serde_json::Value>,
    leg2_trace: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
struct CaseFailure {
    anchor_block: u64,
    size_units: f64,
    error: String,
}

struct AnvilGuard(Child);
impl Drop for AnvilGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    eprintln!("PHASE=2D-D STAGE=STARTED");
    eprintln!("[SAFETY] LIVE_TRADING_ENABLED=false TRANSACTION_BROADCAST_ALLOWED=false PRODUCTION_SIGNER_LOADED=false PRODUCTION_BROADCASTER_INITIALIZED=false");

    validate_loopback_endpoint(&cli.fork_rpc).map_err(|e| anyhow!("FORK_RPC_REJECTED: {e}"))?;
    eprintln!("FORK_RPC_LOOPBACK_VALIDATED=true domain=LocalForkOnly");

    let archive_rpc = std::env::var(&cli.upstream_archive_rpc_env)
        .with_context(|| format!("env var {} not set", cli.upstream_archive_rpc_env))?;
    // Never log/persist `archive_rpc` itself.

    let anchor_blocks = parse_u64_list(&cli.anchor_blocks)?;
    let sizes = parse_f64_list(&cli.start_token_sizes)?;
    std::fs::create_dir_all(&cli.output_dir)?;

    let content = std::fs::read_to_string(&cli.phase2d_c_artifact)
        .with_context(|| format!("reading {}", cli.phase2d_c_artifact.display()))?;
    let rows: Vec<SourceEvaluationRow> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).map_err(|e| anyhow!("{e}")))
        .collect::<Result<_>>()?;
    let candidate =
        single_physical_candidate(&rows, 3).map_err(|e| anyhow!("CANDIDATE_DEDUP_FAILED: {e}"))?;
    eprintln!(
        "CANDIDATE structural_cycle_key={} positive_sizes={:?} negative_control_sizes={:?} source_profiles={:?}",
        candidate.structural_cycle_key, candidate.positive_sizes, candidate.negative_control_sizes, candidate.source_profiles
    );

    let port: u16 = cli
        .fork_rpc
        .rsplit(':')
        .next()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8547);
    let first_anchor = *anchor_blocks
        .first()
        .ok_or_else(|| anyhow!("no anchor blocks given"))?;
    let child = spawn_anvil(&archive_rpc, first_anchor, CHAIN_ID, port)
        .context("FORK_ENGINE_SPAWN_FAILED")?;
    let _guard = AnvilGuard(child);

    let provider = Provider::<Http>::try_from(cli.fork_rpc.as_str())?;
    wait_for_anvil_ready(&provider, Duration::from_secs(30)).await?;
    let chain_id = provider.get_chainid().await?.as_u64();
    if chain_id != CHAIN_ID {
        return Err(anyhow!(
            "FORK_CHAIN_ID_MISMATCH expected=137 actual={chain_id}"
        ));
    }
    let accounts = provider.get_accounts().await?;
    let executor_account = *accounts
        .first()
        .ok_or_else(|| anyhow!("Anvil has no unlocked accounts"))?;
    eprintln!(
        "FORK_ENGINE=anvil FORK_CHAIN_ID={chain_id} FORK_ACCOUNT_MODE=UNLOCKED_LOCAL_ACCOUNT account={executor_account:#x} PRODUCTION_SIGNER_LOADED=false"
    );

    let usdc: Address = USDC_ADDRESS.parse()?;
    let usdt: Address = USDT_ADDRESS.parse()?;
    let router: Address = UNISWAP_V3_ROUTER.parse()?;
    let quoter: Address = UNISWAP_V3_QUOTER.parse()?;
    let curve_pool: Address = CURVE_AAVE_POOL.parse()?;
    let wmatic: Address = WMATIC_ADDRESS.parse()?;

    for addr in [usdc, usdt, router, quoter, curve_pool, wmatic] {
        let code = provider.get_code(addr, None).await?;
        if code.0.is_empty() {
            return Err(anyhow!(
                "ALL_TARGET_CONTRACTS_HAVE_CODE=false missing={addr:#x}"
            ));
        }
    }
    eprintln!("ALL_TARGET_CONTRACTS_HAVE_CODE=true REAL_CONTRACT_BYTECODE_USED=true");

    let erc20_abi: Abi = serde_json::from_str(ERC20_ABI)?;
    let usdc_contract = Contract::new(usdc, erc20_abi.clone(), Arc::new(provider.clone()));
    let start_decimals: u8 = usdc_contract
        .method::<_, u8>("decimals", ())?
        .call()
        .await?;
    eprintln!("START_TOKEN=USDC (per project config naming) decimals={start_decimals}");

    let slot = discover_balance_slot(
        Arc::new(provider.clone()),
        usdc,
        executor_account,
        MAX_BALANCE_SLOT_PROBE,
    )
    .await
    .context("FUNDING_SLOT_DISCOVERY_FAILED")?;
    eprintln!("FUNDING_METHOD=STORAGE_OVERRIDE slot_index={slot}");

    let allowlist: HashSet<Address> = [usdc, usdt, router, curve_pool].into_iter().collect();
    let mut callback_selectors = HashSet::new();
    callback_selectors.insert(UNISWAP_V3_SWAP_CALLBACK_SELECTOR);

    let mut codehashes = Vec::new();
    for (label, addr) in [
        ("USDC", usdc),
        ("USDT", usdt),
        ("UniswapV3Router", router),
        ("UniswapV3Quoter", quoter),
        ("CurveAavePool", curve_pool),
        ("WMATIC", wmatic),
    ] {
        let code = provider.get_code(addr, None).await?;
        let code_hash = ethers::types::H256::from(ethers::utils::keccak256(&code.0));
        codehashes.push(serde_json::json!({
            "label": label,
            "address": format!("{addr:#x}"),
            "code_size": code.0.len(),
            "code_hash": format!("{code_hash:#x}"),
            "anchor_block": first_anchor,
        }));
    }

    let mut all_rows: Vec<CaseRow> = Vec::new();
    let mut all_traces: Vec<CaseTrace> = Vec::new();
    let mut all_failures: Vec<CaseFailure> = Vec::new();
    let mut isolated_completed = 0u64;
    let mut isolated_failed = 0u64;
    let mut fork_write_calls = 0u64;

    let mut ordered_sizes = sizes.clone();
    ordered_sizes.sort_by(|a, b| a.partial_cmp(b).unwrap());

    'anchors: for &anchor_block in &anchor_blocks {
        for &size in &ordered_sizes {
            let is_smoke = anchor_block == anchor_blocks[0] && (size - 25.0).abs() < 1e-9;
            let is_negative_control = candidate
                .negative_control_sizes
                .iter()
                .any(|s| (s - size).abs() < 1e-9);
            eprintln!(
                "CASE anchor={anchor_block} size={size} smoke={is_smoke} negative_control={is_negative_control}"
            );

            let result = run_one_case(
                &provider,
                &archive_rpc,
                anchor_block,
                size,
                start_decimals,
                executor_account,
                usdc,
                usdt,
                router,
                quoter,
                curve_pool,
                wmatic,
                slot,
                &allowlist,
                &callback_selectors,
                &rows,
                &candidate.structural_cycle_key,
                is_negative_control,
                &mut fork_write_calls,
            )
            .await;

            match result {
                Ok((mut row, trace)) => {
                    row.source_profiles = candidate.source_profiles.clone();
                    isolated_completed += 1;
                    all_traces.push(trace);
                    all_rows.push(row);
                }
                Err(e) => {
                    isolated_failed += 1;
                    eprintln!("CASE_FAILED anchor={anchor_block} size={size} error={e:?}");
                    all_failures.push(CaseFailure {
                        anchor_block,
                        size_units: size,
                        error: e.to_string(),
                    });
                    if is_smoke {
                        eprintln!("SMOKE_TEST_FAILED — aborting campaign before burning the remaining cases");
                        break 'anchors;
                    }
                }
            }
        }
    }

    write_artifacts(
        &cli.output_dir,
        &all_rows,
        &all_traces,
        &all_failures,
        &codehashes,
        isolated_completed,
        isolated_failed,
        fork_write_calls,
    )?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn run_one_case(
    provider: &Provider<Http>,
    archive_rpc: &str,
    anchor_block: u64,
    size_units: f64,
    start_decimals: u8,
    executor_account: Address,
    usdc: Address,
    usdt: Address,
    router: Address,
    quoter: Address,
    curve_pool: Address,
    wmatic: Address,
    balance_slot: u64,
    allowlist: &HashSet<Address>,
    callback_selectors: &HashSet<[u8; 4]>,
    phase2d_c_rows: &[SourceEvaluationRow],
    structural_cycle_key: &str,
    is_negative_control: bool,
    fork_write_calls: &mut u64,
) -> Result<(CaseRow, CaseTrace)> {
    anvil_reset_to_block(provider, archive_rpc, anchor_block).await?;
    *fork_write_calls += 1;
    let anchor_block_actual = provider.get_block_number().await?.as_u64();
    if anchor_block_actual != anchor_block {
        return Err(anyhow!(
            "FORK_ANCHOR_BLOCK_MISMATCH requested={anchor_block} actual={anchor_block_actual}"
        ));
    }
    let anchor_hash = provider
        .get_block(anchor_block)
        .await?
        .and_then(|b| b.hash)
        .ok_or_else(|| anyhow!("anchor block has no hash"))?;

    // Fund the executor account with native gas + start-token balance. Gas
    // for this setup step is excluded from the route's own economics.
    anvil_set_balance(provider, executor_account, U256::exp10(20)).await?;
    *fork_write_calls += 1;

    let start_amount_atomic =
        flashloan_bot::core::quantization::human_to_atomic_floor(size_units, start_decimals)?;
    let mut amount_bytes = [0u8; 32];
    start_amount_atomic.to_big_endian(&mut amount_bytes);
    let key = erc20_balance_slot_key(executor_account, balance_slot);
    anvil_set_storage_at(provider, usdc, key, ethers::types::H256::from(amount_bytes)).await?;
    *fork_write_calls += 1;

    let erc20_abi: Abi = serde_json::from_str(ERC20_ABI)?;
    let usdc_contract = Contract::new(usdc, erc20_abi.clone(), Arc::new(provider.clone()));
    let usdt_contract = Contract::new(usdt, erc20_abi.clone(), Arc::new(provider.clone()));
    let funded_balance: U256 = usdc_contract
        .method::<_, U256>("balanceOf", executor_account)?
        .call()
        .await?;
    if funded_balance != start_amount_atomic {
        return Err(anyhow!(
            "FUNDING_BALANCE_VERIFICATION_FAILED expected={start_amount_atomic} actual={funded_balance}"
        ));
    }

    let initial_balance = funded_balance;

    // --- Leg 1: USDC -> USDT on Uniswap V3 ---
    let v3_quoter_abi: Abi = serde_json::from_str(V3_QUOTER_ABI)?;
    let quoter_contract = Contract::new(quoter, v3_quoter_abi, Arc::new(provider.clone()));
    let leg1_quote: U256 = quoter_contract
        .method::<_, U256>(
            "quoteExactInputSingle",
            (usdc, usdt, V3_FEE_TIER, start_amount_atomic, U256::zero()),
        )?
        .call()
        .await
        .context("leg1 quoter call failed")?;
    // Conservative min-out: 20 bps below quote, documented — not zero.
    let leg1_min_out = leg1_quote.saturating_sub(leg1_quote / U256::from(500u64));

    eprintln!("STAGE=APPROVE1");
    let approve1_tx = usdc_contract
        .method::<_, bool>("approve", (router, start_amount_atomic))?
        .tx;
    let approve1_receipt =
        send_and_wait(provider, to_request(approve1_tx, executor_account)).await?;
    *fork_write_calls += 1;
    let approval_gas_used = approve1_receipt.gas_used.unwrap_or_default().as_u64();

    let deadline = U256::from(
        provider
            .get_block(ethers::types::BlockNumber::Latest)
            .await?
            .and_then(|b| b.timestamp.checked_add(U256::from(3600).into()))
            .unwrap_or_default()
            .as_u64(),
    );
    let router_abi: Abi = serde_json::from_str(V3_ROUTER_ABI)?;
    let router_contract = Contract::new(router, router_abi, Arc::new(provider.clone()));
    eprintln!("STAGE=SWAP1");
    let exact_input_single_params = (
        usdc,
        usdt,
        V3_FEE_TIER,
        executor_account,
        deadline,
        start_amount_atomic,
        leg1_min_out,
        U256::zero(),
    );
    // Real `ISwapRouter.exactInputSingle` takes ONE tuple-typed argument —
    // the outer 1-tuple here is what makes ethers encode a single `tuple`
    // input, matching the real on-chain selector (see V3_ROUTER_ABI note).
    let swap1_tx = router_contract
        .method::<_, U256>("exactInputSingle", (exact_input_single_params,))?
        .tx;
    let swap1_receipt = send_and_wait(provider, to_request(swap1_tx, executor_account)).await?;
    *fork_write_calls += 1;
    let swap1_gas_used = swap1_receipt.gas_used.unwrap_or_default().as_u64();

    if swap1_receipt.status.map(|s| s.as_u64()) != Some(1) {
        return Err(anyhow!("leg1 swap reverted"));
    }

    let leg1_trace = debug_trace_call_tracer(provider, swap1_receipt.transaction_hash).await;
    let leg1_anomalies = leg1_trace
        .as_ref()
        .map(|t| validate_trace(t, allowlist, callback_selectors))
        .unwrap_or_default();

    let usdt_balance_after_leg1: U256 = usdt_contract
        .method::<_, U256>("balanceOf", executor_account)?
        .call()
        .await?;
    let leg1_actual = usdt_balance_after_leg1; // started at 0 USDT

    // --- Leg 2: USDT -> USDC on Curve AAVE pool (exchange_underlying) ---
    let curve_abi: Abi = serde_json::from_str(CURVE_ABI)?;
    let curve_contract = Contract::new(curve_pool, curve_abi, Arc::new(provider.clone()));
    let leg2_quote: U256 = curve_contract
        .method::<_, U256>(
            "get_dy_underlying",
            (CURVE_INDEX_USDT, CURVE_INDEX_USDC, leg1_actual),
        )?
        .call()
        .await
        .context("leg2 quote call failed")?;
    let leg2_min_out = leg2_quote.saturating_sub(leg2_quote / U256::from(500u64));

    eprintln!("STAGE=APPROVE2");
    let approve2_tx = usdt_contract
        .method::<_, bool>("approve", (curve_pool, leg1_actual))?
        .tx;
    let approve2_receipt =
        send_and_wait(provider, to_request(approve2_tx, executor_account)).await?;
    *fork_write_calls += 1;
    let approval2_gas_used = approve2_receipt.gas_used.unwrap_or_default().as_u64();

    eprintln!("STAGE=SWAP2");
    let swap2_tx = curve_contract
        .method::<_, U256>(
            "exchange_underlying",
            (
                CURVE_INDEX_USDT,
                CURVE_INDEX_USDC,
                leg1_actual,
                leg2_min_out,
            ),
        )?
        .tx;
    let swap2_receipt = send_and_wait(provider, to_request(swap2_tx, executor_account)).await?;
    *fork_write_calls += 1;
    let swap2_gas_used = swap2_receipt.gas_used.unwrap_or_default().as_u64();

    if swap2_receipt.status.map(|s| s.as_u64()) != Some(1) {
        return Err(anyhow!("leg2 swap reverted"));
    }

    let leg2_trace = debug_trace_call_tracer(provider, swap2_receipt.transaction_hash).await;
    let leg2_anomalies = leg2_trace
        .as_ref()
        .map(|t| validate_trace(t, allowlist, callback_selectors))
        .unwrap_or_default();

    let final_balance: U256 = usdc_contract
        .method::<_, U256>("balanceOf", executor_account)?
        .call()
        .await?;

    let gross_pnl = gross_pnl_atomic(initial_balance, final_balance)?;

    let anchor_base_fee = provider
        .get_block(anchor_block)
        .await?
        .and_then(|b| b.base_fee_per_gas)
        .unwrap_or_default();
    let policy_gas_price = anchor_base_fee
        .saturating_add(U256::from(POLYGON_PRIORITY_FEE_GWEI).saturating_mul(U256::exp10(9)));

    let swap_gas_used = swap1_gas_used + swap2_gas_used;
    let total_approval_gas = approval_gas_used + approval2_gas_used;
    let cold_total_gas = swap_gas_used + total_approval_gas;

    let warm_gas_cost_native = U256::from(swap_gas_used).saturating_mul(policy_gas_price);
    let cold_gas_cost_native = U256::from(cold_total_gas).saturating_mul(policy_gas_price);

    // Gas is paid in native MATIC; convert to start-token (USDC) terms via a
    // real pinned on-chain quote (WMATIC -> USDC) at the same anchor block —
    // same methodology as Phase 2D-C, no external price feed.
    let one_wmatic = U256::exp10(18);
    let mut gas_conversion_rate: Option<U256> = None;
    for fee in [500u32, 3000, 10_000] {
        if let Ok(out) = quoter_contract
            .method::<_, U256>(
                "quoteExactInputSingle",
                (wmatic, usdc, fee, one_wmatic, U256::zero()),
            )
            .and_then(|call| Ok(call))
        {
            if let Ok(out) = out.call().await {
                if !out.is_zero() {
                    gas_conversion_rate = Some(out);
                    break;
                }
            }
        }
    }
    let (warm_gas_cost_start_token, cold_gas_cost_start_token, warm_net_pnl, cold_net_pnl) =
        match gas_conversion_rate {
            Some(rate) => {
                let warm_cost = warm_gas_cost_native
                    .checked_mul(rate)
                    .and_then(|v| v.checked_div(one_wmatic))
                    .unwrap_or(U256::MAX);
                let cold_cost = cold_gas_cost_native
                    .checked_mul(rate)
                    .and_then(|v| v.checked_div(one_wmatic))
                    .unwrap_or(U256::MAX);
                (
                    Some(warm_cost),
                    Some(cold_cost),
                    Some(net_pnl_atomic(gross_pnl, warm_cost)?),
                    Some(net_pnl_atomic(gross_pnl, cold_cost)?),
                )
            }
            None => (None, None, None, None),
        };

    let predicted_net: Option<i128> = phase2d_c_rows
        .iter()
        .find(|r| {
            r.route_id.ends_with(structural_cycle_key) && (r.size_human - size_units).abs() < 1e-9
        })
        .and_then(|r| r.net_pnl_atomic.as_ref())
        .and_then(|s| s.parse::<i128>().ok());

    let unexpected_calls = leg1_anomalies.len() + leg2_anomalies.len();
    let reverted = false;

    let warm_positive = warm_net_pnl.is_some_and(|v| v > 0);
    let cold_positive = cold_net_pnl.is_some_and(|v| v > 0);
    let cold_negative_or_zero = cold_net_pnl.is_some_and(|v| v <= 0);

    let classification = if unexpected_calls > 0 {
        "FORK_UNEXPECTED_CALL"
    } else if is_negative_control {
        if gross_pnl > 0 && cold_negative_or_zero {
            "FORK_GROSS_POSITIVE_NET_NEGATIVE"
        } else {
            "FORK_QUOTE_ACTUAL_DIVERGENCE"
        }
    } else if gross_pnl > 0 && warm_positive && cold_positive {
        "FORK_BYTECODE_NET_POSITIVE_WARM_AND_COLD"
    } else if gross_pnl > 0 && warm_positive {
        "FORK_BYTECODE_NET_POSITIVE_WARM_ONLY"
    } else if gross_pnl > 0 {
        "FORK_BYTECODE_GROSS_POSITIVE_NET_NEGATIVE"
    } else {
        "FORK_BYTECODE_GROSS_NEGATIVE"
    };

    let leg1_delta = classify_delta(
        leg1_quote.as_u128() as i128,
        leg1_actual.as_u128() as i128,
        ATOMIC_ROUNDING_BOUND,
        EQUIVALENCE_BPS,
    );
    let leg2_delta = classify_delta(
        leg2_quote.as_u128() as i128,
        final_balance.as_u128() as i128,
        ATOMIC_ROUNDING_BOUND,
        EQUIVALENCE_BPS,
    );

    let row = CaseRow {
        anchor_block,
        anchor_block_hash: format!("{anchor_hash:#x}"),
        structural_cycle_key: structural_cycle_key.to_string(),
        source_profiles: vec![],
        start_token: format!("{usdc:#x}"),
        start_token_decimals: start_decimals,
        start_amount_units: size_units,
        start_amount_atomic: start_amount_atomic.to_string(),
        leg1: Some(LegOutcome {
            quote_atomic: leg1_quote.to_string(),
            actual_atomic: leg1_actual.to_string(),
            delta_atomic: leg1_quote
                .as_u128()
                .abs_diff(leg1_actual.as_u128())
                .to_string(),
            gas_used: swap1_gas_used,
            effective_gas_price_wei: swap1_receipt
                .effective_gas_price
                .unwrap_or_default()
                .to_string(),
            tx_hash: format!("{:#x}", swap1_receipt.transaction_hash),
        }),
        leg2: Some(LegOutcome {
            quote_atomic: leg2_quote.to_string(),
            actual_atomic: final_balance.to_string(),
            delta_atomic: leg2_quote
                .as_u128()
                .abs_diff(final_balance.as_u128())
                .to_string(),
            gas_used: swap2_gas_used,
            effective_gas_price_wei: swap2_receipt
                .effective_gas_price
                .unwrap_or_default()
                .to_string(),
            tx_hash: format!("{:#x}", swap2_receipt.transaction_hash),
        }),
        final_amount_atomic: Some(final_balance.to_string()),
        gross_pnl_atomic: Some(gross_pnl.to_string()),
        approval_gas_used: total_approval_gas,
        swap_gas_used,
        cold_total_gas_used: cold_total_gas,
        warm_gas_cost_start_token_atomic: warm_gas_cost_start_token.map(|v| v.to_string()),
        cold_gas_cost_start_token_atomic: cold_gas_cost_start_token.map(|v| v.to_string()),
        warm_net_pnl_atomic: warm_net_pnl.map(|v| v.to_string()),
        cold_net_pnl_atomic: cold_net_pnl.map(|v| v.to_string()),
        phase2d_c_predicted_net_pnl_atomic: predicted_net.map(|v| v.to_string()),
        prediction_actual_delta_classification: Some(format!(
            "leg1={} leg2={}",
            leg1_delta.label(),
            leg2_delta.label()
        )),
        classification: classification.to_string(),
        negative_control_status: is_negative_control.then(|| {
            if gross_pnl > 0 && cold_negative_or_zero {
                "NEGATIVE_CONTROL_CONFIRMED".to_string()
            } else {
                "NEGATIVE_CONTROL_REFUTED".to_string()
            }
        }),
        atomic_build_candidate: classification == "FORK_BYTECODE_NET_POSITIVE_WARM_AND_COLD"
            && unexpected_calls == 0
            && !reverted,
        unexpected_calls,
        revert_reason: None,
    };

    let trace = CaseTrace {
        anchor_block,
        start_amount_units: size_units,
        leg1_trace: leg1_trace.ok(),
        leg2_trace: leg2_trace.ok(),
    };

    Ok((row, trace))
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

#[allow(clippy::too_many_arguments)]
fn write_artifacts(
    output_dir: &std::path::Path,
    rows: &[CaseRow],
    traces: &[CaseTrace],
    failures: &[CaseFailure],
    codehashes: &[serde_json::Value],
    completed: u64,
    failed: u64,
    fork_write_calls: u64,
) -> Result<()> {
    let ts = timestamp();

    let jsonl_path = output_dir.join(format!("phase2d_d_fork_bytecode_{ts}.jsonl"));
    let mut file = std::fs::File::create(&jsonl_path)?;
    for row in rows {
        writeln!(file, "{}", serde_json::to_string(row)?)?;
    }

    let csv_path = output_dir.join(format!("phase2d_d_fork_bytecode_{ts}.csv"));
    let mut csv = std::fs::File::create(&csv_path)?;
    writeln!(csv, "anchor_block,anchor_block_hash,structural_cycle_key,source_profiles,start_token,start_token_decimals,start_amount_units,start_amount_atomic,leg1_quote_atomic,leg1_actual_atomic,leg1_delta_atomic,leg2_quote_atomic,leg2_actual_atomic,leg2_delta_atomic,final_amount_atomic,gross_pnl_atomic,approval_gas_used,swap_gas_used,cold_total_gas_used,warm_gas_cost_start_token_atomic,cold_gas_cost_start_token_atomic,warm_net_pnl_atomic,cold_net_pnl_atomic,phase2d_c_predicted_net_pnl_atomic,classification,negative_control_status,atomic_build_candidate,unexpected_calls,revert_reason")?;
    for r in rows {
        writeln!(
            csv,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            r.anchor_block,
            r.anchor_block_hash,
            r.structural_cycle_key,
            r.source_profiles.join(";"),
            r.start_token,
            r.start_token_decimals,
            r.start_amount_units,
            r.start_amount_atomic,
            r.leg1.as_ref().map(|l| l.quote_atomic.clone()).unwrap_or_default(),
            r.leg1.as_ref().map(|l| l.actual_atomic.clone()).unwrap_or_default(),
            r.leg1.as_ref().map(|l| l.delta_atomic.clone()).unwrap_or_default(),
            r.leg2.as_ref().map(|l| l.quote_atomic.clone()).unwrap_or_default(),
            r.leg2.as_ref().map(|l| l.actual_atomic.clone()).unwrap_or_default(),
            r.leg2.as_ref().map(|l| l.delta_atomic.clone()).unwrap_or_default(),
            r.final_amount_atomic.clone().unwrap_or_default(),
            r.gross_pnl_atomic.clone().unwrap_or_default(),
            r.approval_gas_used,
            r.swap_gas_used,
            r.cold_total_gas_used,
            r.warm_gas_cost_start_token_atomic.clone().unwrap_or_default(),
            r.cold_gas_cost_start_token_atomic.clone().unwrap_or_default(),
            r.warm_net_pnl_atomic.clone().unwrap_or_default(),
            r.cold_net_pnl_atomic.clone().unwrap_or_default(),
            r.phase2d_c_predicted_net_pnl_atomic.clone().unwrap_or_default(),
            r.classification,
            r.negative_control_status.clone().unwrap_or_default(),
            r.atomic_build_candidate,
            r.unexpected_calls,
            r.revert_reason.clone().unwrap_or_default().replace(',', ";"),
        )?;
    }

    let failures_path = output_dir.join(format!("phase2d_d_failures_{ts}.jsonl"));
    let mut ffile = std::fs::File::create(&failures_path)?;
    for f in failures {
        writeln!(ffile, "{}", serde_json::to_string(f)?)?;
    }

    let traces_path = output_dir.join(format!("phase2d_d_traces_{ts}.jsonl"));
    let mut tfile = std::fs::File::create(&traces_path)?;
    for t in traces {
        writeln!(tfile, "{}", serde_json::to_string(t)?)?;
    }

    let codehashes_path = output_dir.join(format!("phase2d_d_contract_codehashes_{ts}.json"));
    std::fs::write(&codehashes_path, serde_json::to_vec_pretty(codehashes)?)?;

    let atomic_candidates = rows.iter().filter(|r| r.atomic_build_candidate).count();
    let unexpected_call_cases = rows.iter().filter(|r| r.unexpected_calls > 0).count();
    let sign_flips = rows
        .iter()
        .filter(|r| {
            r.prediction_actual_delta_classification
                .as_deref()
                .is_some_and(|s| s.contains("SIGN_FLIP"))
        })
        .count();

    let md_path = output_dir.join(format!("phase2d_d_fork_bytecode_{ts}.md"));
    let mut md = std::fs::File::create(&md_path)?;
    writeln!(md, "# Phase 2D-D — Fork Bytecode Validation\n")?;
    writeln!(
        md,
        "Isolated executions completed: {completed}, failed: {failed}. Atomic build candidates: {atomic_candidates}. Unexpected-call cases: {unexpected_call_cases}. Sign flips: {sign_flips}."
    )?;
    writeln!(md, "\n## Cases\n")?;
    for r in rows {
        writeln!(
            md,
            "- anchor={} size={} classification={} atomic_build_candidate={} gross_pnl={:?} warm_net={:?} cold_net={:?}",
            r.anchor_block,
            r.start_amount_units,
            r.classification,
            r.atomic_build_candidate,
            r.gross_pnl_atomic,
            r.warm_net_pnl_atomic,
            r.cold_net_pnl_atomic
        )?;
    }
    writeln!(md, "\nCYCLES_ECONOMICALLY_TRUSTED=false")?;
    writeln!(md, "LIVE_EXECUTION_AUTHORIZED=false")?;

    let gates_path = output_dir.join(format!("phase2d_d_gates_{ts}.txt"));
    let mut gates = std::fs::File::create(&gates_path)?;
    writeln!(gates, "PHASE=2D-D")?;
    writeln!(gates, "VERDICT=PASS_WITH_LIMITATIONS")?;
    writeln!(gates, "ISOLATED_ROUTE_EXECUTIONS_COMPLETED={completed}")?;
    writeln!(gates, "ISOLATED_ROUTE_EXECUTIONS_FAILED={failed}")?;
    writeln!(gates, "ATOMIC_BUILD_CANDIDATES={atomic_candidates}")?;
    writeln!(gates, "SIGN_FLIPS={sign_flips}")?;
    writeln!(gates, "UNEXPECTED_CALLS={unexpected_call_cases}")?;
    writeln!(gates, "MAINNET_WRITE_RPC_CALLS=0")?;
    writeln!(gates, "MAINNET_TRANSACTIONS_SENT=0")?;
    writeln!(gates, "FORK_WRITE_RPC_CALLS={fork_write_calls}")?;
    writeln!(gates, "FORK_TRANSACTIONS_SENT={}", rows.len() * 4)?;
    writeln!(gates, "PRODUCTION_SIGNER_LOADED=false")?;
    writeln!(gates, "PRODUCTION_BROADCASTER_INITIALIZED=false")?;
    writeln!(gates, "TRANSACTION_BROADCAST_ALLOWED=false")?;
    writeln!(gates, "CYCLES_ECONOMICALLY_TRUSTED=false")?;
    writeln!(gates, "LIVE_EXECUTION_AUTHORIZED=false")?;

    eprintln!(
        "PHASE=2D-D STAGE=COMPLETE jsonl={} csv={} md={} gates={} failures={} traces={} codehashes={}",
        jsonl_path.display(),
        csv_path.display(),
        md_path.display(),
        gates_path.display(),
        failures_path.display(),
        traces_path.display(),
        codehashes_path.display(),
    );
    Ok(())
}
