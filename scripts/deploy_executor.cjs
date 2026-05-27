// ============================================================
// 🚀 DEPLOY EXECUTOR (FlashloanExecutor v4.4.3-final-OZ5-fixed)
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
  const block = [`[${ts}] [deploy_executor]`, ...lines, ""].join("\n");
  fs.appendFileSync(file, block, "utf8");
}

async function main() {
  if (!process.env.RPC_POLYGON_URL) throw new Error("❌ Falta RPC_POLYGON_URL no .env");
  if (!process.env.PRIVATE_KEY) throw new Error("❌ Falta PRIVATE_KEY no .env");

  const [deployer] = await ethers.getSigners();
  const networkInfo = await ethers.provider.getNetwork();

  console.log("==============================================");
  console.log("🚀", c.cyan("Deploying FlashloanExecutor (v4.4.3-final-OZ5-fixed)"));
  console.log("🌐 Network:", `${networkInfo.name} (chainId=${networkInfo.chainId})`);
  console.log("👤 Deployer:", c.yellow(deployer.address));
  console.log("==============================================");

  // ✅ Deploy do novo contrato com execução direta habilitada
  const wrapperAddress = "0x0000000000000000000000000000000000000000"; // pode ser alterado se houver wrapper externo
  const Executor = await ethers.getContractFactory("FlashloanExecutor");
  const executor = await Executor.deploy(wrapperAddress);

  // 🧩 Compatível com Ethers v6.x
  const tx = await executor.deploymentTransaction();
  const receipt = await tx.wait();
  const addr = await executor.getAddress();

  console.log(c.green("✅ Executor deployed successfully!"));
  console.log("📦 Address:", c.cyan(addr));
  console.log("🔗 TxHash: ", c.yellow(tx.hash));
  console.log("🔎 Explorer:", `https://polygonscan.com/tx/${tx.hash}`);

  // 📊 Registrar no log local
  logToFile([
    `network=${networkInfo.name}`,
    `chainId=${networkInfo.chainId}`,
    `deployer=${deployer.address}`,
    `executor=${addr}`,
    `txHash=${tx.hash}`,
  ]);

  // 🧾 Instrução para atualizar .env
  console.log("\n💾 Copie para o seu .env:");
  console.log(c.dim(`EXECUTOR_ADDRESS=${addr}`));
  console.log(c.dim(`WRAPPER_ADDRESS=${wrapperAddress}`));
  console.log("\n✅ Deploy finalizado com sucesso!");
}

// 🔧 Execução segura
main().catch((err) => {
  console.error(c.red("❌ Deployment failed:"), err);
  process.exit(1);
});
