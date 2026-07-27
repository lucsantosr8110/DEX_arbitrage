// Read-only sanity check before a reduced-capital first real execution.
// Does NOT send any transaction: no approve, no transfer, no swap.
// Usage: node scripts/sanity_check_readonly.cjs [mockCapitalUsd] [deployBlock]
//
// Checks:
//   1. owner() on the deployed FlashloanExecutor matches the configured wallet.
//   2. Every ERC20 Approval ever emitted by the executor (since its deploy
//      block) went to one of the three known routers — not to some other
//      spender. The constructor grants max allowance to QuickSwap/SushiSwap/
//      UniswapV3 for USDC/USDC.e/DAI/WMATIC/USDT by design (see
//      _setMaxAllowancesRoutersOnly in FlashloanExecutor.sol) — that shows up
//      here as expected events, not a red flag. A spender outside that set
//      would be.
//   3. No token balance sitting in the executor contract — funds should only
//      ever pass through inside a single flashloan tx, never rest there.
//   4. A rough off-chain minProfit floor for a mock route, combining the
//      configured Aave premium (fee_pct), slippage tolerance and current
//      gas price — NOT a substitute for the contract's own on-chain checks,
//      just a suggested floor to eyeball before wiring real capital.

require("dotenv").config();
const { ethers } = require("ethers");

const EXECUTOR_ADDRESS = process.env.EXECUTOR_ADDRESS;
const EXPECTED_OWNER = "0x152Aa7ecC490860115C4d1369a19C970f9e9eFFf";
const DEFAULT_DEPLOY_BLOCK = 90949362; // block of the current EXECUTOR_ADDRESS deploy tx

const KNOWN_ROUTERS = new Set(
  [
    "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff", // QuickSwap
    "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506", // SushiSwap
    "0xE592427A0AEce92De3Edee1F18E0157C05861564", // Uniswap V3
  ].map((a) => a.toLowerCase())
);

const TOKENS = {
  WMATIC: "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270",
  USDC: "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359",
  "USDC.e": "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174",
  USDT: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F",
  WETH: "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619",
  DAI: "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063",
  WBTC: "0x1BFD67037B42Cf73acF2047067bd4F2C47D9BfD6",
};

const ERC20_ABI = [
  "function balanceOf(address account) view returns (uint256)",
  "function decimals() view returns (uint8)",
  "event Approval(address indexed owner, address indexed spender, uint256 value)",
];
const EXECUTOR_ABI = ["function owner() view returns (address)"];

const AAVE_FLASHLOAN_PREMIUM_PCT = 0.05; // Aave V3 Polygon, 5 bps, matches fee_pct in config
const SLIPPAGE_TOLERANCE_PCT = 0.5; // matches [flashloan].slippage_tolerance_pct in config
const DEPLOY_GAS_ESTIMATE = 4_622_102n; // from predeploy_views.cjs, used as a stand-in tx-cost estimate

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

let anyFailure = false;
function fail(msg) {
  anyFailure = true;
  console.error(`\x1b[31m❌ ${msg}\x1b[0m`);
}
function ok(msg) {
  console.log(`\x1b[32m✅ ${msg}\x1b[0m`);
}
function warn(msg) {
  console.warn(`\x1b[33m⚠️  ${msg}\x1b[0m`);
}

async function main() {
  if (!EXECUTOR_ADDRESS || !ethers.isAddress(EXECUTOR_ADDRESS)) {
    fail("EXECUTOR_ADDRESS ausente/inválido no .env");
    process.exitCode = 1;
    return;
  }
  if (!process.env.RPC_POLYGON_URL) {
    fail("RPC_POLYGON_URL ausente no .env");
    process.exitCode = 1;
    return;
  }

  const deployBlock = Number(process.argv[3] || DEFAULT_DEPLOY_BLOCK);
  // batchMaxCount: 1 — evita que o ethers agrupe várias chamadas num único
  // POST; provedores como Infura contam isso como "too many requests" por
  // item do batch, não por HTTP request.
  const provider = new ethers.JsonRpcProvider(process.env.RPC_POLYGON_URL, undefined, {
    batchMaxCount: 1,
  });
  const network = await provider.getNetwork();
  console.log(`Rede: chainId=${network.chainId}`);
  if (Number(network.chainId) !== 137) {
    fail(`chainId ${network.chainId} != 137 (Polygon PoS) — abortando`);
    process.exitCode = 1;
    return;
  }

  console.log(`Executor: ${EXECUTOR_ADDRESS}`);

  // 1. owner()
  const executor = new ethers.Contract(EXECUTOR_ADDRESS, EXECUTOR_ABI, provider);
  try {
    const owner = await executor.owner();
    if (owner.toLowerCase() === EXPECTED_OWNER.toLowerCase()) {
      ok(`owner() == ${owner} (esperado)`);
    } else {
      fail(`owner() ${owner} != esperado ${EXPECTED_OWNER}`);
    }
  } catch (e) {
    fail(`owner() falhou: ${e.message}`);
  }

  // 2. Approval events emitidos pelo executor desde o deploy — todo spender
  // deve ser um dos 3 routers conhecidos. Mais forte que assumir "esperado
  // zero", porque o construtor legitimamente pré-aprova max a esses routers.
  console.log(`\nApproval events emitidos pelo executor desde o bloco ${deployBlock}:`);
  const approvalTopic = ethers.id("Approval(address,address,uint256)");
  const ownerTopic = ethers.zeroPadValue(EXECUTOR_ADDRESS, 32);
  let totalApprovals = 0;
  let unexpectedSpenders = 0;

  for (const [symbol, tokenAddr] of Object.entries(TOKENS)) {
    const token = new ethers.Contract(tokenAddr, ERC20_ABI, provider);
    try {
      const logs = await provider.getLogs({
        address: tokenAddr,
        topics: [approvalTopic, ownerTopic],
        fromBlock: deployBlock,
        toBlock: "latest",
      });
      for (const log of logs) {
        totalApprovals++;
        const parsed = token.interface.parseLog(log);
        const spender = parsed.args.spender.toLowerCase();
        const value = parsed.args.value;
        if (KNOWN_ROUTERS.has(spender)) {
          ok(
            `${symbol}: approve(${spender}, ${
              value === ethers.MaxUint256 ? "MAX" : value.toString()
            }) — router conhecido, block ${log.blockNumber}`
          );
        } else {
          unexpectedSpenders++;
          fail(
            `${symbol}: approve(${spender}, ${value.toString()}) — SPENDER DESCONHECIDO, block ${log.blockNumber}`
          );
        }
      }
      if (logs.length === 0) {
        console.log(`  ${symbol}: nenhum Approval emitido`);
      }
    } catch (e) {
      warn(`${symbol}: getLogs falhou (${e.message})`);
    }
    await sleep(200);
  }

  if (unexpectedSpenders === 0 && totalApprovals > 0) {
    ok(`\nTodos os ${totalApprovals} approvals emitidos foram para routers conhecidos.`);
  } else if (totalApprovals === 0) {
    warn("\nNenhum Approval encontrado — confirme se deployBlock está correto.");
  }

  // 3. Saldos do executor (esperado: tudo zero — nenhum fundo deve ficar parado)
  console.log("\nSaldos do executor (esperado: tudo zero):");
  for (const [symbol, tokenAddr] of Object.entries(TOKENS)) {
    const token = new ethers.Contract(tokenAddr, ERC20_ABI, provider);
    let decimals = 18;
    try {
      decimals = await token.decimals();
    } catch {
      /* alguns tokens antigos podem não expor decimals(); segue com 18 */
    }
    try {
      const bal = await token.balanceOf(EXECUTOR_ADDRESS);
      if (bal > 0n) {
        fail(`${symbol}: saldo residual no executor = ${ethers.formatUnits(bal, decimals)}`);
      } else {
        ok(`${symbol}: saldo = 0`);
      }
    } catch (e) {
      warn(`${symbol}: balanceOf falhou (${e.message})`);
    }
    await sleep(200);
  }
  const nativeBal = await provider.getBalance(EXECUTOR_ADDRESS);
  if (nativeBal > 0n) {
    fail(`MATIC nativo residual no executor = ${ethers.formatEther(nativeBal)}`);
  } else {
    ok("MATIC nativo: saldo = 0");
  }

  // 4. Estimativa off-chain de minProfit para uma rota mock
  const mockCapitalUsd = Number(process.argv[2] || 10);
  const feeData = await provider.getFeeData();
  const gasPrice = feeData.gasPrice ?? 0n;
  const gasCostMatic = Number(ethers.formatEther(DEPLOY_GAS_ESTIMATE * gasPrice));

  const premiumUsd = (mockCapitalUsd * AAVE_FLASHLOAN_PREMIUM_PCT) / 100;
  const slippageUsd = (mockCapitalUsd * SLIPPAGE_TOLERANCE_PCT) / 100;
  const suggestedMinProfitUsd = premiumUsd + slippageUsd;

  console.log(`\nEstimativa off-chain (rota mock, capital=$${mockCapitalUsd}):`);
  console.log(`  premium Aave (${AAVE_FLASHLOAN_PREMIUM_PCT}%): $${premiumUsd.toFixed(4)}`);
  console.log(`  buffer slippage (${SLIPPAGE_TOLERANCE_PCT}%): $${slippageUsd.toFixed(4)}`);
  console.log(
    `  gas estimado (a preço atual, ~4.6M gas de referência): ${gasCostMatic.toFixed(
      6
    )} MATIC (não convertido a USD)`
  );
  console.log(
    `  minProfit sugerido (sem gas): >= $${suggestedMinProfitUsd.toFixed(4)} + custo de gas em MATIC`
  );
  warn(
    "Isto é uma estimativa aritmética off-chain, não uma chamada on-chain — valide contra o preço real de gas/MATIC antes de fixar minProfit em produção."
  );

  console.log(anyFailure ? "\n❌ Sanity check encontrou problema(s)." : "\n✅ Sanity check passou.");
  if (anyFailure) process.exitCode = 1;
}

main().catch((e) => {
  fail(`Erro inesperado: ${e.message}`);
  process.exit(1);
});
