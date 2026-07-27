// View-only post-deploy / preflight checks. No swaps / flashloans / approvals.
const { ethers } = require("hardhat");

async function main() {
  const net = await ethers.provider.getNetwork();
  const chainId = Number(net.chainId);
  console.log("chainId", chainId);
  if (chainId !== 137) throw new Error(`expected 137, got ${chainId}`);

  const [signer] = await ethers.getSigners();
  const bal = await ethers.provider.getBalance(signer.address);
  console.log("deployer", signer.address);
  console.log("balance_matic", ethers.formatEther(bal));

  const executor = process.env.EXECUTOR_ADDRESS;
  const wrapper = process.env.WRAPPER_ADDRESS;
  if (executor && ethers.isAddress(executor)) {
    const code = await ethers.provider.getCode(executor);
    console.log("old_executor", executor, "code_bytes", (code.length - 2) / 2);
  }
  if (wrapper && ethers.isAddress(wrapper)) {
    const code = await ethers.provider.getCode(wrapper);
    console.log("wrapper", wrapper, "code_bytes", (code.length - 2) / 2);
  }

  const Executor = await ethers.getContractFactory("FlashloanExecutor");
  const w = wrapper || ethers.ZeroAddress;
  const tx = await Executor.getDeployTransaction(w);
  const gas = await ethers.provider.estimateGas(tx);
  const fee = await ethers.provider.getFeeData();
  console.log("estimateGas", gas.toString());
  console.log("gasPrice", fee.gasPrice?.toString() || "auto");
  if (fee.gasPrice) {
    console.log("est_cost_matic", ethers.formatEther(gas * fee.gasPrice));
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
