// ============================================================
// 🚀 SCRIPT DE APROVAÇÃO DE TOKENS PARA O NOVO EXECUTOR
// ============================================================

require("dotenv").config();
const { ethers } = require("hardhat");

const NEW_EXECUTOR_ADDRESS = "0xC5B79075178866C2B29225AA0c1418464d503a08";
const MAX_ALLOWANCE = ethers.MaxUint256; // Valor máximo para aprovação infinita

// Endereços dos tokens ERC-20 na Polygon Mainnet
const TOKENS_TO_APPROVE = [
    { symbol: "WMATIC", address: "0x0d500B1d8E8eF31E21C99d1Db9A6444d3ADf1270" },
    { symbol: "USDC", address: "0x2791Bca1f2de4661ED88A30C99a7a9449Aa84174" },
    { symbol: "DAI", address: "0x8f3Cf7ad23Cd3CaDbD9735AFf958023239c6A063" },
    { symbol: "USDT", address: "0xc2132D05D31c914a87C6611C10748AEb04B58e8F" },
    { symbol: "WETH", address: "0x7ceB23fD6bC0adD59E62ac25578270cFf1b9f619" },
];

async function main() {
    console.log("==============================================");
    console.log("🔑 Iniciando Aprovação de Tokens para Novo Executor");
    console.log(`Executor: ${NEW_EXECUTOR_ADDRESS}`);
    console.log("==============================================");

    const [signer] = await ethers.getSigners();
    console.log(`👤 Signer: ${signer.address}`);

    const erc20Abi = [
        "function approve(address spender, uint256 amount) returns (bool)",
        "function allowance(address owner, address spender) view returns (uint256)"
    ];

    for (const token of TOKENS_TO_APPROVE) {
        const tokenContract = new ethers.Contract(token.address, erc20Abi, signer);
        
        try {
            // 1. Verificar o allowance atual
            const currentAllowance = await tokenContract.allowance(signer.address, NEW_EXECUTOR_ADDRESS);
            
            // Se o allowance atual for maior que 10 trilhões, consideramos que é suficiente e pulamos a transação.
            // Para ser exato, verificar se é o MAX_ALLOWANCE é ideal, mas essa checagem é mais rápida.
            if (currentAllowance >= 10n * 10n**18n) { // Exemplo de verificação de 'suficiente' (10 trilhões)
                console.log(`✅ ${token.symbol}: Já possui allowance suficiente. Pulando aprovação.`);
                continue; 
            }

            // 2. Enviar a transação de aprovação
            console.log(`➡️ Enviando 'approve' para ${token.symbol} (Max)...`);
            const tx = await tokenContract.approve(NEW_EXECUTOR_ADDRESS, MAX_ALLOWANCE);
            
            console.log(`🔗 Tx Hash: ${tx.hash}`);
            await tx.wait(); // Esperar a confirmação
            
            console.log(`👍 ${token.symbol}: Aprovação confirmada!`);
            
        } catch (error) {
            console.error(`❌ ERRO ao aprovar ${token.symbol}:`, error.message);
        }
    }
    
    console.log("\n🏁 Todas as aprovações foram processadas.");
    console.log("Próximo passo: Reinicie o Bot!");
}

main().catch((error) => {
    console.error(error);
    process.exit(1);
});