# Phase 1 — Pipeline Observability Report

## 1. Environment

- OS: Linux (workspace host)
- Rust: `rustc 1.96.1 (31fca3adb 2026-06-26)`
- Branch: `fix/execution-safety-r2`
- Base commit: `855fad466fa561c4347f8e68c6803e3dd160abda`
- RPC network: Polygon, configured through project environment/configuration; live endpoint not exercised in this phase
- Diagnostic mode: opt-in through `ARBITRAGE_DIAGNOSTIC_MODE=1`; banner added
- Live trading status: not enabled by diagnostic artifacts; no scan process started

## 2. Pipeline Map

```text
generate_full_pair_list (src/dex/radar.rs)
  -> quote_amount_for_usd (src/dex/mod.rs)
  -> PRICE_FEED.get_price (src/infra/price_feed.rs)
  -> DexManager::get_prices_multicall (src/dex/manager.rs)
  -> V2/V3/Curve adapter quote paths
  -> prune_non_reciprocal / quote validation (src/dex/radar.rs)
  -> price map assembly
  -> extract_edges / economic filters (src/dex/radar.rs)
  -> PriceGraph::from_price_map (src/core/bf_graph.rs)
  -> find_arbitrage_cycles (src/core/bf_graph.rs)
  -> profit, gas, slippage and simulation gates
  -> opportunity/bot path
```

- Pair generation discards tokens outside configured/curated allowlists.
- Price sizing discards non-finite/non-positive feed prices, invalid notional, and raw amounts outside range.
- Feed errors currently preserve existing fallback behavior, but now classify/log provider failure and fallback usage.
- DEX collection discards RPC failures, missing pools, invalid quotes, V3 unsupported fee tiers, and failed liquidity/reciprocity checks.
- Graph construction discards malformed pairs and non-finite/non-positive rates; it does not synthesize reverse edges.
- Bellman-Ford returns only cycles inside configured spread bounds.
- Downstream economic and execution gates remain unchanged and fail closed before broadcast.

## 3. Instrumentation Implemented

- `PipelineCounters`: per scan counters for feed, sizing, DEX quotes, price map, graph, Bellman-Ford, filters, and emitted opportunities.
- `QuoteRejectReason`: explicit classification, including timeout, 429, parse, unknown symbol, invalid price, RPC, liquidity, reciprocity, V3, graph, profit, gas, simulation, and route failures.
- `[PIPELINE_SUMMARY]` and `[PIPELINE_REJECTIONS]` emitted on scan completion.
- Rejection details aggregated and sorted; individual rejection logs sampled at debug level.
- Price-feed/sizing debug records include symbol, decimals, notional, source, cache status, price, amount, outcome, and reason. Secrets/API keys are not logged.
- `ARBITRAGE_DIAGNOSTIC_MODE=1` prints read-only safety banner.
- Pure sizing helper added for deterministic tests; no pricing source or economic threshold replaced.

## 4. Real Scan Results

No live scan was started. The current executable contains transaction-capable paths, so an end-to-end run requires an explicitly reviewed dry-run/read-only harness and deployment-specific credentials/configuration. Therefore, live funnel values are `N/A`, not zero.

## 5. Deterministic Test Results

- QuickSwap: `USDC-WMATIC=7.14`
- Uniswap: `WMATIC-USDC=0.145`
- Vertices: `2`
- Directed edges: `2`
- Product: `1.0353` (observed within tolerance)
- Spread: `3.53%` (observed within tolerance)
- Candidate cycle: found

## 6. P1 Verdict

`P1_NOT_CONFIRMED` after Phase 1B real read-only scan.

Primary feed succeeded for all five configured tokens; all sizing and quote attempts completed; price map contained 30 directed quotes. Bellman-Ford found no cycle.

## 7. Other Bottlenecks Found

- Existing live feed path has fallback prices; fallback use is now counted explicitly and must be separated from real-feed success during live analysis.
- Existing counters had duplicate feed/sizing updates; sizing now owns sizing success/failure accounting.
- Live RPC, reciprocity, liquidity, V3, and downstream filter rates remain unmeasured until a safe read-only run.

## 8. Safety

- Transactions sent: `0`
- Live trading enabled: `false` for this diagnostic work
- Secrets exposure: no secrets added to diagnostics or logs
- Reverse synthetic quotes: not added
- Economic filters: not relaxed

## 9. Recommended Next Step

Run one explicitly read-only, dry-run scan with transaction broadcast hard-disabled, capture `[PIPELINE_SUMMARY]` and `[PIPELINE_REJECTIONS]`, then classify P1 from measured feed/sizing/quote funnel values.
