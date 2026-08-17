//! Phase 2D-D fork execution primitives.
//!
//! `erc20_balance_slot_key` is pure and unit-tested; everything else here is
//! thin wrapping around Anvil-specific JSON-RPC methods
//! (`anvil_reset`, `anvil_setStorageAt`, `debug_traceTransaction`) and is
//! exercised by the integration tests / campaign binary against a real,
//! locally-spawned Anvil instance — there is no meaningful way to unit-test
//! "does Anvil actually reset its fork" without Anvil itself.

use anyhow::{anyhow, Context, Result};
use ethers::{
    abi::Abi,
    contract::Contract,
    providers::{Http, Middleware, Provider},
    types::{Address, TransactionReceipt, TransactionRequest, H256, U256},
    utils::keccak256,
};
use serde_json::json;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

const ERC20_BALANCE_OF_ABI: &str = r#"[{"inputs":[{"internalType":"address","name":"account","type":"address"}],"name":"balanceOf","outputs":[{"internalType":"uint256","name":"","type":"uint256"}],"stateMutability":"view","type":"function"}]"#;

/// Storage slot key for `mapping(address => uint256)` at `slot_index`,
/// per Solidity's standard storage layout: `keccak256(pad32(account) ++
/// pad32(slot_index))`. Used to empirically discover (never assume) which
/// slot an ERC-20's balance mapping lives at.
pub fn erc20_balance_slot_key(account: Address, slot_index: u64) -> H256 {
    let mut buf = [0u8; 64];
    buf[12..32].copy_from_slice(account.as_bytes());
    U256::from(slot_index).to_big_endian(&mut buf[32..64]);
    H256::from(keccak256(buf))
}

/// Spawns a local Anvil instance forking `archive_rpc` at `fork_block`,
/// listening only on `127.0.0.1:port`. The caller owns the returned
/// `Child` and must keep it alive for the process's lifetime.
pub fn spawn_anvil(archive_rpc: &str, fork_block: u64, chain_id: u64, port: u16) -> Result<Child> {
    Command::new("anvil")
        .args([
            "--fork-url",
            archive_rpc,
            "--fork-block-number",
            &fork_block.to_string(),
            "--chain-id",
            &chain_id.to_string(),
            "--host",
            "127.0.0.1",
            "--port",
            &port.to_string(),
            "--silent",
            "--fork-retry-backoff",
            "1000",
            "--retries",
            "10",
            "--timeout",
            "30000",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("failed to spawn anvil — is it installed and on PATH?")
}

/// Polls `eth_blockNumber` until Anvil answers or `timeout` elapses.
pub async fn wait_for_anvil_ready(provider: &Provider<Http>, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if provider.get_block_number().await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(anyhow!("ANVIL_NOT_READY after {timeout:?}"));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Resets the already-running Anvil instance's fork to `block_number`,
/// discarding all state mutated since the last reset — this is the
/// per-(anchor_block, size) isolation mechanism (spec section 11), cheaper
/// than respawning the whole process 27 times.
pub async fn anvil_reset_to_block(
    provider: &Provider<Http>,
    archive_rpc: &str,
    block_number: u64,
) -> Result<()> {
    let params = json!([{
        "forking": {
            "jsonRpcUrl": archive_rpc,
            "blockNumber": block_number,
        }
    }]);
    provider
        .request::<_, serde_json::Value>("anvil_reset", params)
        .await
        .context("anvil_reset failed")?;
    Ok(())
}

/// Overrides a single storage slot on the local fork only.
pub async fn anvil_set_storage_at(
    provider: &Provider<Http>,
    address: Address,
    slot: H256,
    value: H256,
) -> Result<()> {
    provider
        .request::<_, serde_json::Value>("anvil_setStorageAt", json!([address, slot, value]))
        .await
        .context("anvil_setStorageAt failed")?;
    Ok(())
}

pub async fn anvil_set_balance(
    provider: &Provider<Http>,
    address: Address,
    wei: U256,
) -> Result<()> {
    provider
        .request::<_, serde_json::Value>("anvil_setBalance", json!([address, wei]))
        .await
        .context("anvil_setBalance failed")?;
    Ok(())
}

/// Empirically discovers the storage slot index of `account`'s balance in
/// an ERC-20 `mapping(address => uint256)` by writing a probe value to
/// candidate slots and reading it back via `balanceOf` — never assumes a
/// slot layout. Returns the first index (0..=`max_slot`) that round-trips.
pub async fn discover_balance_slot(
    provider: Arc<Provider<Http>>,
    token: Address,
    account: Address,
    max_slot: u64,
) -> Result<u64> {
    let abi: Abi = serde_json::from_str(ERC20_BALANCE_OF_ABI)?;
    let contract = Contract::new(token, abi, provider.clone());
    let probe = U256::from(123_456_789_u64);
    for slot_index in 0..=max_slot {
        let key = erc20_balance_slot_key(account, slot_index);
        let mut probe_bytes = [0u8; 32];
        probe.to_big_endian(&mut probe_bytes);
        anvil_set_storage_at(&provider, token, key, H256::from(probe_bytes)).await?;
        let observed: U256 = contract
            .method::<_, U256>("balanceOf", account)?
            .call()
            .await
            .unwrap_or_default();
        if observed == probe {
            return Ok(slot_index);
        }
    }
    Err(anyhow!(
        "BALANCE_SLOT_NOT_DISCOVERED token={token:#x} account={account:#x} tried=0..={max_slot}"
    ))
}

/// Sends a transaction via `eth_sendTransaction` against Anvil's own
/// unlocked account (no local signer, no private key material touches this
/// process) and waits for the receipt.
pub async fn send_and_wait(
    provider: &Provider<Http>,
    tx: TransactionRequest,
) -> Result<TransactionReceipt> {
    let pending = provider
        .send_transaction(tx, None)
        .await
        .context("eth_sendTransaction failed")?;
    pending
        .await
        .context("waiting for receipt failed")?
        .ok_or_else(|| anyhow!("transaction dropped, no receipt"))
}

/// `debug_traceTransaction` with the `callTracer` tracer, as a raw
/// `serde_json::Value` for `fork_trace_validation` to walk.
pub async fn debug_trace_call_tracer(
    provider: &Provider<Http>,
    tx_hash: H256,
) -> Result<serde_json::Value> {
    provider
        .request(
            "debug_traceTransaction",
            json!([tx_hash, { "tracer": "callTracer" }]),
        )
        .await
        .context("debug_traceTransaction failed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slot_key_is_deterministic() {
        let a = Address::from_low_u64_be(1);
        assert_eq!(erc20_balance_slot_key(a, 9), erc20_balance_slot_key(a, 9));
    }

    #[test]
    fn slot_key_differs_by_account() {
        let a = Address::from_low_u64_be(1);
        let b = Address::from_low_u64_be(2);
        assert_ne!(erc20_balance_slot_key(a, 9), erc20_balance_slot_key(b, 9));
    }

    #[test]
    fn slot_key_differs_by_slot_index() {
        let a = Address::from_low_u64_be(1);
        assert_ne!(erc20_balance_slot_key(a, 9), erc20_balance_slot_key(a, 10));
    }

    #[test]
    fn slot_key_is_32_bytes() {
        let key = erc20_balance_slot_key(Address::from_low_u64_be(1), 0);
        assert_eq!(key.as_bytes().len(), 32);
    }
}
