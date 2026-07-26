// Configura USDC nativo Polygon no executor existente, sem redeploy.
// Idempotente: só envia tx se configuração atual divergir.
require("dotenv").config();
const { ethers } = require("hardhat");

const NATIVE_USDC = "0x3c499c542cEF5E3811e1192ce70d8cC03d5c3359";
const BASE_SLIPPAGE_BPS = 100;
const MAX_SLIPPAGE_BPS = 300;

async function main() {
  const executorAddress = process.env.EXECUTOR_ADDRESS;
  if (!executorAddress) throw new Error("EXECUTOR_ADDRESS ausente no .env");

  const [signer] = await ethers.getSigners();
  const executor = await ethers.getContractAt("FlashloanExecutor", executorAddress);
  const owner = await executor.owner();
  if (owner.toLowerCase() !== signer.address.toLowerCase()) {
    throw new Error(`signer ${signer.address} não é owner ${owner}`);
  }

  const current = await executor.slippageConfigs(NATIVE_USDC);
  const configured =
    current.baseSlippage === BigInt(BASE_SLIPPAGE_BPS) &&
    current.maxSlippage === BigInt(MAX_SLIPPAGE_BPS) &&
    current.enabled;

  console.log("executor:", executorAddress);
  console.log("native USDC:", NATIVE_USDC);
  console.log("current:", current);
  if (configured) {
    console.log("USDC nativo já configurado; nenhuma transação enviada.");
    return;
  }

  const tx = await executor.updateSlippageConfig(
    NATIVE_USDC,
    BASE_SLIPPAGE_BPS,
    MAX_SLIPPAGE_BPS,
    true,
  );
  console.log("tx:", tx.hash);
  const receipt = await tx.wait();
  console.log("confirmada no bloco:", receipt.blockNumber);
}

main().catch((error) => {
  console.error("Falha:", error.shortMessage || error.message || error);
  process.exit(1);
});
