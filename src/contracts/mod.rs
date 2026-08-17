// ============================================================
// src/contracts/mod.rs â€” (VersÃ£o Final e Correta)
// ============================================================

use ethers::prelude::abigen;

// ============================================================
// 1. Contratos Principais de Arbitragem
// ============================================================

// Executor (CÃ©rebro)
abigen!(
    FlashloanExecutor,
    "abi/FlashloanExecutor.json", // âœ… sem sufixo de versÃ£o
);

// Caller (Wrapper/Carteira)
abigen!(FlashloanCaller, "abi/FlashloanCaller.json",);

// ============================================================
// 2. Interfaces V3 (Uniswap)
// ============================================================

// Quoter V2 (Principal)
abigen!(QuoterV3V2, "abi/uniswap_v3_quoter_v2.json",);

// Quoter V1 (Fallback)
abigen!(QuoterV3, "abi/uniswap_v3_quoter.json",);

// Router V3 (Interface)
abigen!(UniswapV3Router, "abi/IUniswapV3Router.json",);

// ============================================================
// 3. Interfaces V2 (Quickswap / Sushiswap / Uniswap)
// ============================================================

// Router Sushiswap
abigen!(SushiswapRouter, "abi/sushiswap_router.json",);

// Router Quickswap
abigen!(QuickswapRouter, "abi/quickswap_router.json",);

// Router Uniswap V2 (GenÃ©rico)
abigen!(UniswapV2Router, "abi/uniswap_v2_router.json",);

// Factory V2 (para obter pares)
abigen!(UniswapV2Factory, "abi/uniswap_v2_factory.json",);

// Pair V2 (para obter reservas/liquidez)
abigen!(UniswapV2Pair, "abi/uniswap_v2_pair.json",);

// ============================================================
// 4. Interfaces Comuns
// ============================================================

// Token ERC20
abigen!(ERC20, "abi/ERC20.json",);

// Aave Pool Interface
abigen!(AavePool, "abi/IAavePool.json",);
