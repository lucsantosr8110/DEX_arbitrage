//! Phase 2D-D fork integration tests. These spawn a *real* local Anvil
//! instance forking Polygon at a real historical block via a real archive
//! RPC endpoint — there is no meaningful way to test "Anvil resets its fork
//! state" or "the real contracts have bytecode at this historical block"
//! with a mock. Requires `anvil` on PATH and a `POLYGON_ARCHIVE_RPC_URL` env
//! var pointing at an archive-capable Polygon RPC endpoint; fails loudly
//! (not silently skipped) if either is missing, matching this phase's
//! fail-closed / `BLOCKED`-not-`PASS` philosophy.

use ethers::providers::{Http, Middleware, Provider};
use ethers::types::{Address, U256};
use flashloan_bot::core::fork_route_executor::{
    anvil_reset_to_block, anvil_set_balance, spawn_anvil, wait_for_anvil_ready,
};
use std::time::Duration;

const ANCHOR_BLOCK: u64 = 91_149_850;
const CHAIN_ID: u64 = 137;

const USDC: &str = "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359";
const USDT: &str = "0xc2132D05D31c914a87C6611C10748AEb04B58e8F";
const UNISWAP_V3_ROUTER: &str = "0xE592427A0AEce92De3Edee1F18E0157C05861564";
const UNISWAP_V3_QUOTER: &str = "0xb27308f9F90D607463bb33eA1BeBb41C27CE5AB6";
const CURVE_AAVE_POOL: &str = "0x445FE580eF8d70FF569aB36e80c647af338db351";
const WMATIC: &str = "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270";

fn archive_rpc() -> String {
    std::env::var("POLYGON_ARCHIVE_RPC_URL")
        .expect("POLYGON_ARCHIVE_RPC_URL must be set to run Phase 2D-D fork integration tests")
}

struct Guard(std::process::Child);
impl Drop for Guard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[tokio::test]
async fn phase2d_d_smoke_fork_has_real_contract_bytecode_at_anchor_block() {
    let archive = archive_rpc();
    let port = 8646u16;
    let child = spawn_anvil(&archive, ANCHOR_BLOCK, CHAIN_ID, port).expect("anvil spawn failed");
    let _guard = Guard(child);

    let provider = Provider::<Http>::try_from(format!("http://127.0.0.1:{port}"))
        .expect("provider construction failed");
    wait_for_anvil_ready(&provider, Duration::from_secs(30))
        .await
        .expect("anvil never became ready");

    let chain_id = provider.get_chainid().await.unwrap().as_u64();
    assert_eq!(chain_id, CHAIN_ID);

    let block_number = provider.get_block_number().await.unwrap().as_u64();
    assert_eq!(
        block_number, ANCHOR_BLOCK,
        "fork did not land on the requested anchor block"
    );

    for (label, addr) in [
        ("USDC", USDC),
        ("USDT", USDT),
        ("UniswapV3Router", UNISWAP_V3_ROUTER),
        ("UniswapV3Quoter", UNISWAP_V3_QUOTER),
        ("CurveAavePool", CURVE_AAVE_POOL),
        ("WMATIC", WMATIC),
    ] {
        let address: Address = addr.parse().unwrap();
        let code = provider.get_code(address, None).await.unwrap();
        assert!(
            !code.0.is_empty(),
            "{label} ({addr}) has no bytecode on the fork"
        );
    }
}

#[tokio::test]
async fn phase2d_d_reset_restores_anchor_state() {
    let archive = archive_rpc();
    let port = 8647u16;
    let child = spawn_anvil(&archive, ANCHOR_BLOCK, CHAIN_ID, port).expect("anvil spawn failed");
    let _guard = Guard(child);

    let provider = Provider::<Http>::try_from(format!("http://127.0.0.1:{port}"))
        .expect("provider construction failed");
    wait_for_anvil_ready(&provider, Duration::from_secs(30))
        .await
        .expect("anvil never became ready");

    let probe_account: Address = Address::from_low_u64_be(0xff);
    let baseline = provider.get_balance(probe_account, None).await.unwrap();

    let sentinel = U256::exp10(24); // an absurd, unmistakable balance
    anvil_set_balance(&provider, probe_account, sentinel)
        .await
        .expect("anvil_setBalance failed");
    let mutated = provider.get_balance(probe_account, None).await.unwrap();
    assert_eq!(mutated, sentinel, "balance override did not take effect");
    assert_ne!(
        mutated, baseline,
        "sentinel value collided with real baseline balance"
    );

    anvil_reset_to_block(&provider, &archive, ANCHOR_BLOCK)
        .await
        .expect("anvil_reset failed");

    let after_reset = provider.get_balance(probe_account, None).await.unwrap();
    assert_eq!(
        after_reset, baseline,
        "anvil_reset did not restore the pre-mutation anchor-block state"
    );
    assert_ne!(
        after_reset, sentinel,
        "the sentinel mutation leaked across the reset — state isolation is broken"
    );

    let block_number = provider.get_block_number().await.unwrap().as_u64();
    assert_eq!(
        block_number, ANCHOR_BLOCK,
        "reset did not land back on the anchor block"
    );
}

#[tokio::test]
async fn phase2d_d_two_resets_to_same_block_produce_identical_state() {
    // Determinism companion to the isolation test: resetting to the same
    // anchor block twice, with different mutations in between, must yield
    // the same observable state both times — not just "some" state.
    let archive = archive_rpc();
    let port = 8648u16;
    let child = spawn_anvil(&archive, ANCHOR_BLOCK, CHAIN_ID, port).expect("anvil spawn failed");
    let _guard = Guard(child);

    let provider = Provider::<Http>::try_from(format!("http://127.0.0.1:{port}"))
        .expect("provider construction failed");
    wait_for_anvil_ready(&provider, Duration::from_secs(30))
        .await
        .expect("anvil never became ready");

    let usdc: Address = USDC.parse().unwrap();

    anvil_set_balance(&provider, usdc, U256::exp10(10))
        .await
        .unwrap();
    anvil_reset_to_block(&provider, &archive, ANCHOR_BLOCK)
        .await
        .unwrap();
    let code_after_first_reset = provider.get_code(usdc, None).await.unwrap();

    anvil_set_balance(&provider, usdc, U256::exp10(20))
        .await
        .unwrap();
    anvil_reset_to_block(&provider, &archive, ANCHOR_BLOCK)
        .await
        .unwrap();
    let code_after_second_reset = provider.get_code(usdc, None).await.unwrap();

    assert_eq!(
        code_after_first_reset, code_after_second_reset,
        "two resets to the same anchor block produced different contract code"
    );
}
