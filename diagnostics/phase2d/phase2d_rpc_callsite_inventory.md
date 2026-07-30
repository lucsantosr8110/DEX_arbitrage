# Phase 2D-A RPC quote callsite inventory

Scope: quote-producing paths and their historical replay equivalents. Runtime
scanner is `src/bin/read_only_scan.rs`; every accepted quote goes through
`pinned_eth_call` and an explicit `BlockId::Number`.

| arquivo | função | venue | método RPC | executa quote | bloco explícito atualmente | deve ser pinado |
|---|---|---|---|---:|---:|---:|
| `src/bin/read_only_scan.rs` | `quote_v2` | QUICKSWAP_V2 / SUSHISWAP_V2 | `eth_call(getAmountsOut)` | true | true | true |
| `src/bin/read_only_scan.rs` | `quote_v3` | UNISWAP_V3 | `eth_call(quoteExactInputSingle)` por fee tier | true | true | true |
| `src/bin/read_only_scan.rs` | `quote_curve` | CURVE | `eth_call(get_dy)` | true | true | true |
| `src/bin/read_only_scan.rs` | historical probe | TOKEN_METADATA | `eth_call(decimals)` | false | true | true |
| `src/core/replay_scan.rs` | `HistQuoter::quote_curve` | CURVE | `eth_call(get_dy)` | true | true (`block`) | true |
| `src/core/replay_scan.rs` | `HistQuoter::quote_v3` | UNISWAP_V3 | `eth_call(quoteExactInputSingle)` | true | true (`block`) | true |
| `src/core/replay_scan.rs` | `HistQuoter::quote_v2` | QUICKSWAP_V2 / SUSHISWAP_V2 | `eth_call(getAmountsOut)` | true | true (`multicall block`) | true |
| `src/dex/adapters/uniswap_v2.rs` | `get_price` / `get_price_batch` | QUICKSWAP_V2 / SUSHISWAP_V2 | `eth_call(getAmountsOut)` | true | legacy latest path | true |
| `src/dex/adapters/uniswap_v3.rs` | `get_price` / fee-tier calls | UNISWAP_V3 | `eth_call(quoteExactInputSingle)` | true | legacy latest path | true |
| `src/dex/adapters/curve.rs` | `get_price` / stable quote | CURVE | `eth_call(get_dy)` | true | legacy latest path | true |
| `src/dex/get_token_decimals.rs` | `get_token_decimals` | TOKEN_METADATA | `eth_call(decimals)` | false | latest cache path | false |
| `src/core/paper_validation.rs` | `eth_call_raw` | OTHER | raw `eth_call` | false | caller-supplied tag | false |

`QUOTE_RPC_CALLSITES_IDENTIFIED=12`
`UNCLASSIFIED_QUOTE_CALLSITES=0`

Phase 2D-A canaries use the read-only scanner path. Legacy live adapters remain
outside this diagnostic binary and are explicitly marked for pinning before a
future production integration; they are not used by the canaries.
