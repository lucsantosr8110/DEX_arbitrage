// ============================================================
// ⚙️ HARDHAT CONFIG — multi-chain + Polygonscan API
// Versão: 2025-V2-MULTICHAIN (POLYGONSCAN compat.)
// ============================================================

require("dotenv").config();
require("@nomicfoundation/hardhat-toolbox");

// ============================================================
// 🔑 VARIÁVEIS DE AMBIENTE
// ============================================================
const PRIVATE_KEY = process.env.PRIVATE_KEY;
const ALCHEMY_KEY = process.env.ALCHEMY_KEY || "";
const INFURA_KEY = process.env.INFURA_KEY || "";
const POLYGONSCAN_API_KEY = process.env.POLYGONSCAN_API_KEY;

if (!PRIVATE_KEY) {
  console.warn("⚠️  PRIVATE_KEY não definido no .env — deploy/verify podem falhar.");
}

// ============================================================
// 🌐 CONFIGURAÇÕES DE REDE
// ============================================================
module.exports = {
  solidity: {
    version: "0.8.24",
    settings: {
      optimizer: {
        enabled: true,
        runs: 9999,
      },
    },
  },

  networks: {
    // --------------------------
    // Polygon Mainnet
    // --------------------------
    polygon: {
      url:
        process.env.RPC_POLYGON_URL ||
        `https://polygon-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}` ||
        `https://polygon-mainnet.infura.io/v3/${INFURA_KEY}`,
      accounts: PRIVATE_KEY ? [PRIVATE_KEY] : [],
      chainId: 137,
      gasPrice: "auto",
      timeout: 60000,
    },

    // --------------------------
    // Ethereum Mainnet
    // --------------------------
    mainnet: {
      url:
        process.env.RPC_MAINNET_URL ||
        `https://eth-mainnet.g.alchemy.com/v2/${ALCHEMY_KEY}`,
      accounts: PRIVATE_KEY ? [PRIVATE_KEY] : [],
      chainId: 1,
      gasPrice: "auto",
    },

    // --------------------------
    // Base Mainnet
    // --------------------------
    base: {
      url:
        process.env.RPC_BASE_URL || "https://mainnet.base.org",
      accounts: PRIVATE_KEY ? [PRIVATE_KEY] : [],
      chainId: 8453,
      gasPrice: "auto",
    },

    // --------------------------
    // Arbitrum One
    // --------------------------
    arbitrum: {
      url:
        process.env.RPC_ARBITRUM_URL || "https://arb1.arbitrum.io/rpc",
      accounts: PRIVATE_KEY ? [PRIVATE_KEY] : [],
      chainId: 42161,
      gasPrice: "auto",
    },

    // --------------------------
    // Polygon Amoy (testnet)
    // --------------------------
    amoy: {
      url:
        process.env.RPC_AMOY_URL || "https://rpc-amoy.polygon.technology",
      accounts: PRIVATE_KEY ? [PRIVATE_KEY] : [],
      chainId: 80002,
      gasPrice: "auto",
    },

    // --------------------------
    // Hardhat local
    // --------------------------
    hardhat: {
      chainId: 31337,
      forking: process.env.RPC_POLYGON_URL
        ? {
            url: process.env.RPC_POLYGON_URL,
          }
        : undefined,
    },
  },

  // ============================================================
  // 🔍 POLYGONSCAN — suporte direto com variável POLYGONSCAN_API_KEY
  // ============================================================
  etherscan: {
    apiKey: {
      polygon: POLYGONSCAN_API_KEY, // 👈 Mantém nome antigo
      amoy: POLYGONSCAN_API_KEY,    // compatível com testnet
      base: POLYGONSCAN_API_KEY,
      arbitrum: POLYGONSCAN_API_KEY,
      mainnet: POLYGONSCAN_API_KEY,
    },
    customChains: [
      {
        network: "polygon",
        chainId: 137,
        urls: {
          apiURL: "https://api.polygonscan.com/api",
          browserURL: "https://polygonscan.com",
        },
      },
      {
        network: "amoy",
        chainId: 80002,
        urls: {
          apiURL: "https://api-amoy.polygonscan.com/api",
          browserURL: "https://amoy.polygonscan.com",
        },
      },
      {
        network: "base",
        chainId: 8453,
        urls: {
          apiURL: "https://api.basescan.org/api",
          browserURL: "https://basescan.org",
        },
      },
      {
        network: "arbitrum",
        chainId: 42161,
        urls: {
          apiURL: "https://api.arbiscan.io/api",
          browserURL: "https://arbiscan.io",
        },
      },
      {
        network: "mainnet",
        chainId: 1,
        urls: {
          apiURL: "https://api.etherscan.io/api",
          browserURL: "https://etherscan.io",
        },
      },
    ],
  },

  // ============================================================
  // 🧰 OUTRAS CONFIGURAÇÕES
  // ============================================================
  paths: {
    sources: "./contracts",
    tests: "./test",
    cache: "./cache",
    artifacts: "./artifacts",
  },

  mocha: {
    timeout: 100000,
  },
};
