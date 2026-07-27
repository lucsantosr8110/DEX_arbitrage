// ============================================================
// 🚀 DEPLOY EXECUTOR (FlashloanExecutor v4.4.3-final-OZ5-fixed)
// ============================================================

require("dotenv").config();
const fs = require("fs");
const path = require("path");
const { ethers, artifacts } = require("hardhat");

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
  const chainId = Number(networkInfo.chainId);
  if (chainId !== 137) {
    throw new Error(`❌ Refuse deploy: chainId=${chainId} (expected Polygon mainnet 137)`);
  }

  const bal = await ethers.provider.getBalance(deployer.address);
  const oldExecutor = process.env.EXECUTOR_ADDRESS || "(none)";

  console.log("==============================================");
  console.log("🚀", c.cyan("Deploying FlashloanExecutor (Phase2 profitRecipient + V3 fail-closed)"));
  console.log("🌐 Network:", `${networkInfo.name} (chainId=${chainId})`);
  console.log("👤 Deployer/owner:", c.yellow(deployer.address));
  console.log("💰 Balance MATIC:", ethers.formatEther(bal));
  console.log("📦 Previous EXECUTOR_ADDRESS:", c.dim(oldExecutor));
  console.log("==============================================");

  // Preserva o wrapper de produção; o post-deploy também o autoriza e aponta
  // o FlashloanCaller para este novo executor.
  const wrapperAddress = process.env.WRAPPER_ADDRESS || "0x0000000000000000000000000000000000000000";
  if (!ethers.isAddress(wrapperAddress)) throw new Error("❌ WRAPPER_ADDRESS inválido no .env");
  const Executor = await ethers.getContractFactory("FlashloanExecutor");

  // Preflight: bytecode contains Phase-2 markers (custom errors).
  const artifact = await artifacts.readArtifact("FlashloanExecutor");
  const joined = JSON.stringify(artifact.abi);
  if (!joined.includes("UnauthorizedProfitRecipient")) {
    throw new Error("❌ Artifact missing UnauthorizedProfitRecipient — refuse deploy");
  }
  if (!joined.includes("InvalidV3FeeExtraDataLength")) {
    throw new Error("❌ Artifact missing InvalidV3FeeExtraDataLength — refuse deploy");
  }

  const feeData = await ethers.provider.getFeeData();
  const gasPrice = feeData.gasPrice ?? feeData.maxFeePerGas;
  const estimated = await ethers.provider.estimateGas(
    await Executor.getDeployTransaction(wrapperAddress)
  );
  const costWei = estimated * (gasPrice ?? 0n);
  console.log("⛽ estimateGas:", estimated.toString());
  console.log("⛽ gasPrice wei:", gasPrice?.toString() || "(auto)");
  console.log("⛽ est. cost MATIC:", ethers.formatEther(costWei));

  console.log("\n📤 Broadcasting deploy TX…");
  const executor = await Executor.deploy(wrapperAddress);

  // 🧩 Compatível com Ethers v6.x
  const tx = await executor.deploymentTransaction();
  const receipt = await tx.wait();
  const addr = await executor.getAddress();

  // Tudo abaixo (print + log em arquivo) usa só dados do receipt — nada que
  // dependa de uma leitura de estado subsequente (ex.: owner()) pode impedir
  // o registro de uma transação que já foi minerada e já custou gas real.
  console.log(c.green("✅ Executor deployed successfully!"));
  console.log("📦 Address:", c.cyan(addr));
  console.log("🔗 TxHash: ", c.yellow(tx.hash));
  console.log("🧱 Block:  ", receipt.blockNumber);
  console.log("⛽ gasUsed:", receipt.gasUsed?.toString());
  console.log("🔎 Explorer:", `https://polygonscan.com/tx/${tx.hash}`);

  // 📊 Registrar no log local (old + new) — antes de qualquer leitura pós-tx
  // que possa falhar por lag de propagação do RPC (owner() logo abaixo).
  logToFile([
    `network=${networkInfo.name}`,
    `chainId=${chainId}`,
    `deployer=${deployer.address}`,
    `previous_executor=${oldExecutor}`,
    `executor=${addr}`,
    `wrapper=${wrapperAddress}`,
    `txHash=${tx.hash}`,
    `block=${receipt.blockNumber}`,
    `gasUsed=${receipt.gasUsed?.toString()}`,
  ]);

  // 🧾 Instrução para atualizar .env / config (não sobrescreve silenciosamente)
  console.log("\n💾 Register new address (manual / workflow):");
  console.log(c.dim(`# previous: EXECUTOR_ADDRESS=${oldExecutor}`));
  console.log(c.dim(`EXECUTOR_ADDRESS=${addr}`));
  console.log(c.dim(`WRAPPER_ADDRESS=${wrapperAddress}`));

  // Sanity check pós-registro: confirma owner() on-chain. Se essa leitura
  // falhar (ex.: nó do RPC ainda não propagou o bloco), o deploy e o log
  // acima já estão gravados — só avisa, não perde o registro.
  let onchainOwner = null;
  try {
    onchainOwner = await executor.owner();
  } catch (e) {
    console.warn(
      c.yellow(
        `⚠️  owner() read failed right after deploy (likely RPC propagation lag): ${e.message}`
      )
    );
    console.warn(c.yellow("⚠️  Deploy + log já gravados. Rode um owner() read-only separado antes de confiar cegamente."));
  }

  if (onchainOwner !== null && onchainOwner.toLowerCase() !== deployer.address.toLowerCase()) {
    throw new Error(
      `❌ owner() ${onchainOwner} != deployer ${deployer.address} — deploy e log já registrados acima, investigue antes de usar este endereço.`
    );
  }

  if (onchainOwner !== null) {
    console.log("👑 owner():", onchainOwner);
  }

  console.log("\n✅ Deploy finalizado com sucesso!");
}

// 🔧 Execução segura
main().catch((err) => {
  console.error(c.red("❌ Deployment failed:"), err);
  process.exit(1);
});
