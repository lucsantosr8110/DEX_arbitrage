// ============================================================
// 🚀 DEPLOY WRAPPER (FlashloanCaller) — Polygon Mainnet
// • Compatível com Hardhat + Ethers v6
// ============================================================

require("dotenv").config();
const fs = require("fs");
const path = require("path");
const { ethers } = require("hardhat");

const c = {
  green: (s) => `\x1b[32m${s}\x1b[0m`,
  red: (s) => `\x1b[31m${s}\x1b[0m`,
  cyan: (s) => `\x1b[36m${s}\x1b[0m`,
  yellow: (s) => `\x1b[33m${s}\x1b[0m`,
  dim: (s) => `\x1b[2m${s}\x1b[0m`,
};

function logToFile(lines) {
  const dir = path.join(process.cwd(), "logs");
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, "deployments.log");
  const ts = new Date().toISOString();
  const block = [`[${ts}] [deploy_wrapper]`, ...lines, ""].join("\n");
  fs.appendFileSync(file, block, "utf8");
}

async function main() {
  if (!process.env.RPC_POLYGON_URL) throw new Error("❌ Falta RPC_POLYGON_URL no .env");
  if (!process.env.PRIVATE_KEY) throw new Error("❌ Falta PRIVATE_KEY no .env (deployer/owner)");
  if (!process.env.EXECUTOR_ADDRESS) throw new Error("❌ Falta EXECUTOR_ADDRESS no .env");

  const executorAddr = process.env.EXECUTOR_ADDRESS;
  const [deployer] = await ethers.getSigners();
  const networkInfo = await ethers.provider.getNetwork();
  const network = networkInfo.name || "polygon";
  const chainId = networkInfo.chainId;

  console.log("==============================================");
  console.log("🚀", c.cyan("Deploying FlashloanCaller"));
  console.log("🌐 Network:", `${network} (chainId=${chainId})`);
  console.log("👤 Deployer:", c.yellow(deployer.address));
  console.log("⚙️  Executor:", c.cyan(executorAddr));
  console.log("==============================================");

  // Deploy usando Ethers v6
  const Wrapper = await ethers.getContractFactory("FlashloanCaller");
  const wrapper = await Wrapper.deploy(executorAddr);

  const tx = await wrapper.deploymentTransaction();
  const receipt = await tx.wait();
  const addr = await wrapper.getAddress();
  const txHash = tx.hash;

  console.log(c.green("✅ Wrapper deployed successfully!"));
  console.log("📦 Address:", c.cyan(addr));
  console.log("🔗 TxHash: ", c.yellow(txHash));
  console.log("🔎 Explorer:", `https://polygonscan.com/tx/${txHash}`);

  logToFile([
    `network=${network}`,
    `chainId=${chainId}`,
    `deployer=${deployer.address}`,
    `wrapper=${addr}`,
    `executor=${executorAddr}`,
    `txHash=${txHash}`,
  ]);

  console.log("\n💾 Copie para o .env:");
  console.log(c.dim(`WRAPPER_ADDRESS=${addr}`));
}

main().catch((err) => {
  console.error(c.red("❌ Deployment failed:"), err);
  logToFile([`error=${String(err && err.stack || err)}`]);
  process.exit(1);
});
