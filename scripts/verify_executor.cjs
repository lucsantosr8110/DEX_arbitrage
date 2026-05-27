// ============================================================
// 🔍 VERIFY EXECUTOR — FlashloanExecutor (Polygon Mainnet)
// Com argumento do construtor (_wrapperAddress)
// ============================================================

require("dotenv").config();
const fs = require("fs");
const path = require("path");
const { execSync } = require("child_process");

const c = {
  green: (s) => `\x1b[32m${s}\x1b[0m`,
  red:   (s) => `\x1b[31m${s}\x1b[0m`,
  cyan:  (s) => `\x1b[36m${s}\x1b[0m`,
  yellow:(s) => `\x1b[33m${s}\x1b[0m`,
  dim:   (s) => `\x1b[2m${s}\x1b[0m`,
};

const CONTRACT_NAME = "FlashloanExecutor";
const ADDRESS = process.env.EXECUTOR_ADDRESS;
const WRAPPER_ADDRESS =
  process.env.WRAPPER_ADDRESS ||
  "0x0000000000000000000000000000000000000000";
const NETWORK = "polygon";

function logToFile(lines) {
  const dir = path.join(process.cwd(), "logs");
  if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
  const file = path.join(dir, "verify.log");
  const ts = new Date().toISOString();
  const block = [`[${ts}] [verify_executor_with_args]`, ...lines, ""].join("\n");
  fs.appendFileSync(file, block, "utf8");
}

async function main() {
  if (!ADDRESS) throw new Error("❌ EXECUTOR_ADDRESS não definido no .env");
  if (!process.env.POLYGONSCAN_API_KEY)
    throw new Error("❌ Falta POLYGONSCAN_API_KEY no .env");

  console.log("==============================================");
  console.log("🔍", c.cyan(`Verificando contrato ${CONTRACT_NAME}`));
  console.log("📦 Address:", c.yellow(ADDRESS));
  console.log("🧩 Wrapper:", c.yellow(WRAPPER_ADDRESS));
  console.log("🌐 Network:", c.cyan(NETWORK));
  console.log("==============================================");

  try {
    const cmd = `npx hardhat verify --network ${NETWORK} ${ADDRESS} "${WRAPPER_ADDRESS}"`;
    console.log(c.dim(`> ${cmd}\n`));

    const output = execSync(cmd, { encoding: "utf8" });
    console.log(c.green("✅ Verificação enviada ao Polygonscan!\n"));
    console.log(output);

    logToFile([
      `network=${NETWORK}`,
      `contract=${CONTRACT_NAME}`,
      `address=${ADDRESS}`,
      `wrapper=${WRAPPER_ADDRESS}`,
      `result=SUCCESS`,
      `output=${output}`,
    ]);
  } catch (err) {
    console.error(c.red("❌ Falha na verificação:\n"), err.message);
    logToFile([
      `network=${NETWORK}`,
      `contract=${CONTRACT_NAME}`,
      `address=${ADDRESS}`,
      `wrapper=${WRAPPER_ADDRESS}`,
      `result=FAIL`,
      `error=${err.message}`,
    ]);
    process.exit(1);
  }
}

main();
