# Phase 1B — Real Read-Only Scan Report

## 1. Environment

- Branch/base: `fix/execution-safety-r2` / `855fad466fa561c4347f8e68c6803e3dd160abda`
- Network: Polygon, chain ID 137
- Scan: one completed cycle, 11,814 ms

## 2. Worktree and Branch

Pre-existing WIP was preserved. No commit was made because mandatory Clippy gate failed.

## 3. Read-Only Architecture

`src/bin/read_only_scan.rs` creates `Provider<RotatingHttpClient>` directly. It does not import `RpcProvider`, `AppMiddleware`, `SignerMiddleware`, wallet, execution engine, flashloan client, Jito, or broadcaster. It does not load `.env`; shell injected RPC configuration then removed `PRIVATE_KEY` before process start.

## 4. Transaction Path Audit

- `src/infra/rpc_provider.rs`: constructs `SignerMiddleware`; unreachable, not imported.
- `src/core/flashloan.rs`: calls `send_transaction`; unreachable, not imported.
- `src/execution/execution_engine.rs`: signs transactions; unreachable, not imported.
- `src/main.rs`: reads `PRIVATE_KEY` and initializes execution; unreachable, separate binary.

## 5. Safety Guards

Read-only process requires exactly `ARBITRAGE_DIAGNOSTIC_MODE=true`; `LIVE_TRADING_ENABLED=true` and `TRANSACTION_BROADCAST_ALLOWED=true` abort. A real guard test with broadcast=true aborted before RPC.

## 6. Real Scan Configuration

Tokens: USDC, USDT, WMATIC, WETH, WBTC. Notional: existing $100 sizing. One deterministic bounded scan. Quotes are `eth_call` only against QuickSwap V2 and Uniswap V3 quoter.

## 7. Funnel Results

| scan | ms | pairs | feed primary | fallback | sizing | DEX quotes | accepted | map pairs | graph edges | BF cycles | opportunities |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 1 | 11814 | 20 | 5 | 0 | 20 | 40 | 30 | 30 | 30 | 0 | 0 |

Rates: feed success 100%; sizing success 100%; DEX quote success 100%; acceptance 75%; reciprocity rejection 25% (10/40 raw quotes).

## 8. Rejection Reasons

`NonReciprocal=10`. No price-feed failures, invalid values, or fallback use observed.

## 9. P1 Verdict

`P1_NOT_CONFIRMED`: primary feed succeeded for all five configured tokens, sizing and quotes completed, and price map contained 30 pairs.

## 10. First Zero Stage

`FIRST_ZERO_STAGE=BF_CYCLES_RAW`. Graph has 5 vertices and 30 directed edges; Bellman-Ford found no candidate cycle in its configured 0.1%–50% spread range.

## 11. Clippy Corrections

Phase 1B records baseline errors in `phase1b_clippy_before.txt`. Full workspace Clippy fails with 127 pre-existing errors, including old dead code and style lints in untouched execution/config/UI modules. No global lint suppression was added.

## 12. Tests and Validation

- deterministic graph test remains passing in previous full suite
- read-only guard/counter unit tests added
- release read-only binary ran successfully
- full Clippy remains failing

## 13. Safety Evidence

`MAINNET_TRANSACTIONS_SENT=0`; no signer, broadcaster, or Jito initialized; diagnostic output contains no secret values.

## 14. Recommended Next Phase

`FASE_2=REVISAR_BF_CYCLE_RECONSTRUCTION` after first bringing Clippy baseline to zero in a dedicated maintenance change.
