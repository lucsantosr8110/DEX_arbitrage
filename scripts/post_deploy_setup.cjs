// ============================================================
// 🚀 POST-DEPLOY SETUP — FlashloanExecutor + FlashloanCaller
// ------------------------------------------------------------
// Idempotente. Faz:
//   1. authorizeWrapper(WRAPPER, true) + setWrapper(WRAPPER) no executor
//      (necessário para o fluxo wrapper: executeOperation valida initiator)
//   2. approve bot->executor nos 5 tokens (modo Direct: safeTransferFrom)
//
// Lê EXECUTOR_ADDRESS e WRAPPER_ADDRESS do .env.
// Reexecutar é seguro: checa estado antes de enviar tx.
// ============================================================

require("dotenv").config();
const { ethers } = require("hardhat");

const ZERO = "0x0000000000000000000000000000000000000000";

const TOKENS = [
  { symbol: "WMATIC", address: "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270" },
  { symbol: "USDC",   address: "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174" },
  { symbol: "DAI",    address: "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063" },
  { symbol: "USDT",   address: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F" },
  { symbol: "WETH",   address: "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619" },
];

const ERC20_ABI = [
  "function approve(address spender, uint256 amount) returns (bool)",
  "function allowance(address owner, address spender) view returns (uint256)",
];

async function main() {
  const EXEC = process.env.EXECUTOR_ADDRESS;
  const WRAP = process.env.WRAPPER_ADDRESS;
  if (!EXEC) throw new Error("❌ EXECUTOR_ADDRESS ausente no .env");

  const [signer] = await ethers.getSigners();
  const net = await ethers.provider.getNetwork();
  console.log("==============================================");
  console.log("🔧 Post-deploy setup");
  console.log("🌐 chainId:", String(net.chainId));
  console.log("👤 signer: ", signer.address);
  console.log("📦 executor:", EXEC);
  console.log("🎁 wrapper: ", WRAP || "(nenhum)");
  console.log("==============================================");

  const executor = await ethers.getContractAt("FlashloanExecutor", EXEC);

  // ------------------------------------------------------------
  // 1. Wrapper linkage no executor
  // ------------------------------------------------------------
  if (WRAP && WRAP.toLowerCase() !== ZERO.toLowerCase()) {
    // setWrapper
    const curWrap = await executor.wrapperAddress();
    if (curWrap.toLowerCase() !== WRAP.toLowerCase()) {
      console.log("➡️ setWrapper(", WRAP, ")");
      const tx = await executor.setWrapper(WRAP);
      await tx.wait();
      console.log("✅ wrapperAddress atualizado:", tx.hash);
    } else {
      console.log("✅ wrapperAddress já corresponde");
    }

    // authorizeWrapper
    const auth = await executor.authorizedWrappers(WRAP);
    if (!auth) {
      console.log("➡️ authorizeWrapper(", WRAP, ", true)");
      const tx = await executor.authorizeWrapper(WRAP, true);
      await tx.wait();
      console.log("✅ wrapper autorizado:", tx.hash);
    } else {
      console.log("✅ wrapper já autorizado");
    }
  } else {
    console.log("ℹ️ Sem wrapper — pulando linkage (modo flashloan direto)");
  }

  // ------------------------------------------------------------
  // 2. approve bot -> executor (modo Direct: safeTransferFrom)
  // ------------------------------------------------------------
  console.log("\n🔓 Aprovando bot -> executor (modo Direct)...");
  const MAX = ethers.MaxUint256;
  for (const t of TOKENS) {
    const c = new ethers.Contract(t.address, ERC20_ABI, signer);
    const a = await c.allowance(signer.address, EXEC);
    if (a >= 10n * 10n ** 18n) {
      console.log(`✅ ${t.symbol}: allowance suficiente`);
      continue;
    }
    console.log(`➡️ approve ${t.symbol} -> executor (Max)`);
    const tx = await c.approve(EXEC, MAX);
    await tx.wait();
    console.log(`👍 ${t.symbol}: aprovado (${tx.hash})`);
  }

  // ------------------------------------------------------------
  // 3. Sanity: confirmar bot é executor
  // ------------------------------------------------------------
  const isExec = await executor.allowedExecutors(signer.address);
  console.log("\n🧪 bot é executor?", isExec);
  if (!isExec) {
    console.log("➡️ updateExecutor(bot, true)");
    const tx = await executor.updateExecutor(signer.address, true);
    await tx.wait();
    console.log("✅ bot autorizado como executor:", tx.hash);
  }

  console.log("\n🏁 Post-deploy setup concluído.");
}

main().catch((e) => {
  console.error("❌ Falha:", e.message || e);
  process.exit(1);
});