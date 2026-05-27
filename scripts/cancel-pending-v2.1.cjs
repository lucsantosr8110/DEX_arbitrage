// ============================================================
// 🚨 CANCEL PENDING TX v2.3 — Polygon Mainnet (FIXED)
// ------------------------------------------------------------
// ✅ CORRIGIDO: Transação para próprio endereço com valor 0
// ✅ Usa transação para address(0) ou conta vazia
// ✅ Suporte EIP-1559 e Legacy
// ✅ Retry + Fallback RPC robusto
// ============================================================

require("dotenv").config();
const fs = require("fs");
const path = require("path");
const axios = require("axios");
const { ethers } = require("hardhat");

const c = {
    green: (s) => `\x1b[32m${s}\x1b[0m`,
    red: (s) => `\x1b[31m${s}\x1b[0m`,
    yellow: (s) => `\x1b[33m${s}\x1b[0m`,
    cyan: (s) => `\x1b[36m${s}\x1b[0m`,
    dim: (s) => `\x1b[2m${s}\x1b[0m`,
};

function logToFile(lines) {
    const dir = path.join(process.cwd(), "logs");
    if (!fs.existsSync(dir)) fs.mkdirSync(dir, { recursive: true });
    const file = path.join(dir, "cancel_pending.log");
    const ts = new Date().toISOString();
    fs.appendFileSync(file, [`[${ts}]`, ...lines, ""].join("\n"), "utf8");
}

// 🔁 Retry wrapper para chamadas API
async function fetchWithRetry(url, headers = {}, retries = 3) {
    for (let i = 1; i <= retries; i++) {
        try {
            const { data } = await axios.get(url, { headers, timeout: 10000 });
            if (Array.isArray(data)) return data;
            if (data.items) return data.items;
            if (data.result && Array.isArray(data.result)) return data.result;
            throw new Error(`Resposta inesperada: ${JSON.stringify(data).slice(0, 120)}...`);
        } catch (err) {
            console.warn(c.yellow(`⚠️ Tentativa ${i}/${retries} falhou na API: ${err.message}`));
            if (i === retries) throw new Error("❌ Falha na API após 3 tentativas.");
            await new Promise((r) => setTimeout(r, 1500 * i));
        }
    }
}

// ⚙️ Função de obtenção de gás para Ethers v6
async function getNewGasPrice(provider, oldGas) {
    try {
        const feeData = await provider.getFeeData();
        
        // Para Polygon, que usa EIP-1559
        if (feeData.maxFeePerGas && feeData.maxPriorityFeePerGas) {
            const baseMaxFee = feeData.maxFeePerGas;
            const basePriorityFee = feeData.maxPriorityFeePerGas;
            
            if (oldGas && oldGas > 0n) {
                // Aumenta 15% para substituição
                return {
                    maxFeePerGas: (baseMaxFee * 115n) / 100n,
                    maxPriorityFeePerGas: (basePriorityFee * 115n) / 100n
                };
            }
            
            // Taxa normal para fallback
            return {
                maxFeePerGas: baseMaxFee,
                maxPriorityFeePerGas: basePriorityFee
            };
        } else {
            // Fallback para gasPrice legacy
            const baseGas = feeData.gasPrice || 30000000000n; // 30 gwei como fallback
            
            if (oldGas && oldGas > 0n) {
                return { gasPrice: (oldGas * 115n) / 100n };
            }
            
            return { gasPrice: baseGas };
        }
    } catch (error) {
        console.warn(c.yellow("⚠️ Erro ao obter gas price, usando fallback..."));
        return { gasPrice: 40000000000n }; // 40 gwei como fallback absoluto
    }
}

// 🎯 Função para criar transação de cancelamento
function createCancelTransaction(nonce, gasConfig, wallet) {
    // Estratégias de cancelamento (por ordem de preferência):
    
    // 1. Transação para address(0) com dados (mais eficaz)
    const cancelTx1 = {
        to: "0x0000000000000000000000000000000000000000",
        value: 0,
        nonce: nonce,
        gasLimit: 21000,
        data: "0x", // dados vazios
        ...gasConfig
    };
    
    // 2. Transação para um contrato conhecido (opcional)
    const cancelTx2 = {
        to: "0x000000000000000000000000000000000000dEaD", // address dead
        value: 0,
        nonce: nonce,
        gasLimit: 21000,
        ...gasConfig
    };
    
    // 3. Self-transfer com valor 0 (menos preferível)
    const cancelTx3 = {
        to: wallet.address,
        value: 0,
        nonce: nonce,
        gasLimit: 21000,
        ...gasConfig
    };
    
    return cancelTx1; // Usando a estratégia mais confiável
}

async function main() {
    const apiKey = process.env.POLYGONSCAN_API_KEY;
    const privateKey = process.env.PRIVATE_KEY;

    if (!apiKey || !privateKey) {
        console.error(c.red("❌ Falta POLYGONSCAN_API_KEY ou PRIVATE_KEY no .env"));
        process.exit(1);
    }

    const provider = ethers.provider;
    const wallet = new ethers.Wallet(privateKey, provider);
    const address = wallet.address;

    console.log("==============================================");
    console.log("🔍", c.cyan("CANCEL PENDING TXs — Polygon Mainnet (v2.3 - FIXED)"));
    console.log("👤 Address:", c.yellow(address));
    console.log("==============================================");

    // 1. Tentar buscar transações pendentes via API
    const url = `https://api.polygonscan.com/api?module=account&action=txlist&address=${address}&startblock=0&endblock=99999999&page=1&offset=50&sort=asc&apikey=${apiKey}`;
    let txs = [];
    let useFallback = false;

    try {
        const response = await fetchWithRetry(url);
        txs = response;
    } catch (apiErr) {
        console.warn(c.red("⚠️ Falha na API Polygonscan — ativando fallback RPC..."));
        useFallback = true;
    }

    // 2. Lógica de Fallback (se API falhar)
    if (useFallback) {
        try {
            const latest = await provider.getTransactionCount(address, "latest");
            const pending = await provider.getTransactionCount(address, "pending");
            console.log(`📊 Nonce latest=${latest}, pending=${pending}`);

            if (pending === latest) {
                console.log(c.green("✅ Nenhuma transação pendente detectada via RPC."));
                return;
            }

            console.log(c.yellow(`⚠️  Transação pendente detectada no nonce: ${latest}`));
            
            const gasConfig = await getNewGasPrice(provider, 0n);
            const cancelTx = createCancelTransaction(latest, gasConfig, wallet);
            
            console.log("🔄 Enviando transação de cancelamento...");
            const sent = await wallet.sendTransaction(cancelTx);
            console.log("🔗 Hash:", c.yellow(sent.hash));
            
            console.log("⏳ Aguardando confirmação...");
            const receipt = await sent.wait();
            
            if (receipt.status === 1) {
                console.log(c.green("✅ Transação pendente cancelada com sucesso via fallback RPC!"));
                logToFile([
                    `FALLBACK_SUCCESS address=${address}`,
                    `nonce=${latest}`,
                    `txHash=${sent.hash}`,
                    `blockNumber=${receipt.blockNumber}`
                ]);
            } else {
                console.log(c.red("❌ Falha na substituição via fallback RPC."));
            }
            return;
        } catch (e) {
            console.error(c.red("❌ Falha no envio de fallback:"), e.message);
            logToFile([`FALLBACK_ERROR error=${e.message}`]);
            return;
        }
    }
    
    // 3. Lógica de Substituição (se API funcionar)
    const pendingTxs = txs.filter((tx) => tx.blockNumber === "0" || !tx.blockNumber);
    
    if (pendingTxs.length === 0) {
        console.log(c.green("✅ Nenhuma transação pendente detectada (via API)."));
        return;
    }

    console.log(c.yellow(`⚠️  ${pendingTxs.length} transações pendentes encontradas:`));
    console.log(c.dim("Nonce | Gas Price (gwei) | Hash | Idade"));
    console.log(c.dim("----------------------------------------------"));

    const now = Math.floor(Date.now() / 1000);
    pendingTxs.forEach((tx) => {
        const age = now - parseInt(tx.timeStamp || now);
        const gasPriceGwei = ethers.formatUnits(tx.gasPrice || "0", "gwei");
        console.log(
            `${String(tx.nonce).padEnd(5)} | ${gasPriceGwei.slice(0, 8).padEnd(12)} | ${tx.hash.slice(0, 10)}... | ${age}s`
        );
    });

    // Pegar a transação mais antiga (menor nonce)
    const oldest = pendingTxs.reduce((prev, current) => 
        parseInt(prev.nonce) < parseInt(current.nonce) ? prev : current
    );
    
    const nonce = parseInt(oldest.nonce);
    const oldGas = ethers.toBigInt(oldest.gasPrice || "0");
    
    // Calcula novo gás com 15% de aumento
    const newGasConfig = await getNewGasPrice(provider, oldGas);

    console.log("==============================================");
    console.log(c.cyan("🧹 Cancelando transação mais antiga..."));
    console.log(`🔢 Nonce: ${nonce}`);
    console.log(`⛽ Gas antigo: ${ethers.formatUnits(oldGas, "gwei")} gwei`);
    
    if (newGasConfig.maxFeePerGas) {
        console.log(`⚙️  Max Fee: ${ethers.formatUnits(newGasConfig.maxFeePerGas, "gwei")} gwei`);
        console.log(`🎯 Priority Fee: ${ethers.formatUnits(newGasConfig.maxPriorityFeePerGas, "gwei")} gwei`);
    } else {
        console.log(`⚙️  Gas novo: ${ethers.formatUnits(newGasConfig.gasPrice, "gwei")} gwei`);
    }
    
    console.log("==============================================");

    try {
        const cancelTx = createCancelTransaction(nonce, newGasConfig, wallet);
        const sent = await wallet.sendTransaction(cancelTx);
        console.log("📤 Enviando TX de cancelamento...");
        console.log("🔗 Hash:", c.yellow(sent.hash));

        const receipt = await sent.wait();
        
        if (receipt.status === 1) {
            console.log(c.green("✅ Transação pendente substituída/cancelada com sucesso!"));
            console.log(c.green(`📦 Bloco: ${receipt.blockNumber}`));
        } else {
            console.log(c.red("❌ Falha ao substituir a transação."));
        }

        logToFile([
            `API_SUCCESS address=${address}`,
            `nonce=${nonce}`,
            `oldGas=${ethers.formatUnits(oldGas, "gwei")}`,
            `newGas=${newGasConfig.maxFeePerGas ? ethers.formatUnits(newGasConfig.maxFeePerGas, "gwei") : ethers.formatUnits(newGasConfig.gasPrice, "gwei")}`,
            `txHash=${sent.hash}`,
            `status=${receipt.status}`,
            `blockNumber=${receipt.blockNumber}`
        ]);
        
    } catch (error) {
        console.error(c.red("❌ Erro ao enviar transação de cancelamento:"), error.message);
        logToFile([`SEND_ERROR error=${error.message}`]);
        
        // Tentativa de fallback em caso de erro
        if (error.message.includes("replacement") || error.message.includes("nonce")) {
            console.log(c.yellow("🔄 Tentando fallback alternativo..."));
            try {
                const gasConfig = await getNewGasPrice(provider, oldGas * 2n); // Dobra o aumento
                const fallbackTx = createCancelTransaction(nonce, gasConfig, wallet);
                const sent = await wallet.sendTransaction(fallbackTx);
                console.log("🔗 Hash (fallback):", c.yellow(sent.hash));
                await sent.wait();
                console.log(c.green("✅ Fallback executado com sucesso!"));
            } catch (fallbackError) {
                console.error(c.red("❌ Fallback também falhou:"), fallbackError.message);
            }
        }
    }
}

main().catch((err) => {
    console.error(c.red("❌ Cancel Pending v2.3 failed:"), err);
    logToFile([`MAIN_ERROR error=${String(err && err.stack || err)}`]);
    process.exit(1);
});