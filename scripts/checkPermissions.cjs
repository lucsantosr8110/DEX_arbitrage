// ============================================================
// ✅ CHECK PERMISSIONS — valida estado on-chain do par
//   FlashloanExecutor + FlashloanCaller
// ------------------------------------------------------------
// Read-only. Lê EXECUTOR_ADDRESS + WRAPPER_ADDRESS do .env.
// Verifica:
//   • owner / paused do executor
//   • wrapperAddress + authorizedWrappers (linkage wrapper)
//   • allowedExecutors[bot] (bot pode executar)
//   • wrapper.executor aponta pro executor correto
//   • allowances EXEC -> 3 routers para 4 tokens (swaps)
//   • allowances BOT -> EXEC para 4 tokens (modo Direct)
// Saída: tabela + exit 1 se qualquer cheque crítico falhar.
// ============================================================

require("dotenv").config();
const { ethers } = require("hardhat");

const ROUTERS = [
  { name: "QuickSwap",  addr: "0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff" },
  { name: "SushiSwap",  addr: "0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506" },
  { name: "UniswapV3",  addr: "0xE592427A0AEce92De3Edee1F18E0157C05861564" },
];

const TOKENS = [
  { sym: "WMATIC", addr: "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270", dec: 18 },
  { sym: "USDC",   addr: "0x2791Bca1f2de4661ED88A30C99A7a9449Aa84174", dec: 6 },
  { sym: "DAI",    addr: "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063", dec: 18 },
  { sym: "USDT",   addr: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F", dec: 6 },
];

const EXEC_ABI = [
  "function owner() view returns (address)",
  "function paused() view returns (bool)",
  "function wrapperAddress() view returns (address)",
  "function allowedExecutors(address) view returns (bool)",
  "function authorizedWrappers(address) view returns (bool)",
];
const WRAP_ABI = [
  "function owner() view returns (address)",
  "function executor() view returns (address)",
];
const ERC20_ABI = ["function allowance(address,address) view returns (uint256)"];

let failures = 0;
const ok = (cond, label) => {
  console.log(`${cond ? "✅" : "❌"} ${label}`);
  if (!cond) failures++;
};

async function main() {
  const EXEC = process.env.EXECUTOR_ADDRESS;
  const WRAP = process.env.WRAPPER_ADDRESS;
  if (!EXEC) throw new Error("❌ EXECUTOR_ADDRESS ausente no .env");

  const [signer] = await ethers.getSigners();
  const net = await ethers.provider.getNetwork();
  console.log("==============================================");
  console.log("✅ Check Permissions");
  console.log("🌐 chainId:", String(net.chainId));
  console.log("👤 bot:    ", signer.address);
  console.log("📦 executor:", EXEC);
  console.log("🎁 wrapper: ", WRAP || "(nenhum)");
  console.log("==============================================");

  const ex = await ethers.getContractAt("FlashloanExecutor", EXEC);
  const bot = signer.address;

  // --- Executor state ---
  const owner = await ex.owner();
  const paused = await ex.paused();
  ok(owner.toLowerCase() === bot.toLowerCase(), `executor owner == bot (${owner})`);
  ok(!paused, "executor not paused");

  // --- Wrapper linkage ---
  if (WRAP && WRAP.toLowerCase() !== ethers.ZeroAddress) {
    const wAddr = await ex.wrapperAddress();
    const auth = await ex.authorizedWrappers(WRAP);
    ok(wAddr.toLowerCase() === WRAP.toLowerCase(), `wrapperAddress == wrapper (${wAddr})`);
    ok(auth, "authorizedWrappers[wrapper] == true");

    const wr = await ethers.getContractAt("FlashloanCaller", WRAP);
    const wrExec = await wr.executor();
    const wrOwner = await wr.owner();
    ok(wrExec.toLowerCase() === EXEC.toLowerCase(), `wrapper.executor == executor (${wrExec})`);
    ok(wrOwner.toLowerCase() === bot.toLowerCase(), `wrapper.owner == bot (${wrOwner})`);
  } else {
    console.log("ℹ️ Sem wrapper — checagens de linkage puladas");
  }

  // --- Bot is executor ---
  const isExec = await ex.allowedExecutors(bot);
  ok(isExec, "allowedExecutors[bot] == true");

  // --- EXEC -> ROUTER allowances ---
  console.log("\n— EXEC -> ROUTER allowances —");
  for (const t of TOKENS) {
    const c = new ethers.Contract(t.addr, ERC20_ABI, ethers.provider);
    const row = [];
    for (const r of ROUTERS) {
      const a = await c.allowance(EXEC, r.addr);
      row.push(a > 0n ? "OK" : "ZERO");
    }
    const allOk = row.every((x) => x === "OK");
    ok(allOk, `${t.sym} -> [${row.join(", ")}]`);
  }

  // --- BOT -> EXEC allowances (Direct mode) ---
  console.log("\n— BOT -> EXEC allowances (Direct) —");
  for (const t of TOKENS) {
    const c = new ethers.Contract(t.addr, ERC20_ABI, ethers.provider);
    const a = await c.allowance(bot, EXEC);
    ok(a > 0n, `${t.sym} -> executor`);
  }

  console.log("\n==============================================");
  if (failures === 0) {
    console.log("🏁 Tudo OK — permissões válidas.");
  } else {
    console.log(`⚠️  ${failures} cheque(s) falharam.`);
  }
  process.exit(failures === 0 ? 0 : 1);
}

main().catch((e) => {
  console.error("❌ Falha:", e.message || e);
  process.exit(1);
});