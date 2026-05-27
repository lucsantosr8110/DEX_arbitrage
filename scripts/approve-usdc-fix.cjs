// ============================================================
// 🚀 SCRIPT DE APROVAÇÃO USDC - CORREÇÃO ESPECÍFICA (CJS)
// ============================================================

require("dotenv").config();
const { ethers } = require("hardhat");

const NEW_EXECUTOR_ADDRESS = "0xC5B79075178866C2B29225AA0c1418464d503a08";
const MAX_ALLOWANCE = ethers.MaxUint256;

// APENAS USDC - com endereço em lowercase para evitar checksum
const USDC = {
    symbol: "USDC", 
    address: "0x2791bca1f2de4661ed88a30c99a7a9449aa84174" // lowercase
};

async function main() {
    console.log("==============================================");
    console.log("🔑 APROVAÇÃO USDC - CORREÇÃO ESPECÍFICA");
    console.log("Executor:", NEW_EXECUTOR_ADDRESS);
    console.log("==============================================");

    const [signer] = await ethers.getSigners();
    console.log("👤 Signer:", signer.address);
    
    const balance = await signer.provider.getBalance(signer.address);
    console.log("💰 Balance:", ethers.formatEther(balance), "MATIC");

    // ABI simplificada apenas para approve e allowance
    const erc20Abi = [
        "function approve(address spender, uint256 amount) external returns (bool)",
        "function allowance(address owner, address spender) external view returns (uint256)",
        "function balanceOf(address account) external view returns (uint256)",
        "function decimals() external view returns (uint8)"
    ];

    try {
        const tokenContract = new ethers.Contract(USDC.address, erc20Abi, signer);
        
        // 1. Verificar se o contrato USDC é acessível
        console.log("🔍 Verificando contrato USDC...");
        const decimals = await tokenContract.decimals();
        const balance = await tokenContract.balanceOf(signer.address);
        console.log("📊 USDC Balance:", ethers.formatUnits(balance, decimals), "USDC");
        
        // 2. Verificar allowance atual
        const currentAllowance = await tokenContract.allowance(signer.address, NEW_EXECUTOR_ADDRESS);
        console.log("📋 Allowance atual:", ethers.formatUnits(currentAllowance, decimals), "USDC");
        
        if (currentAllowance >= MAX_ALLOWANCE / 2n) {
            console.log("✅ USDC: Já possui allowance máximo/suficiente");
            console.log("💡 Se ainda há erro, tente revogar primeiro: amount = 0");
            return;
        }

        // 3. Enviar aprovação
        console.log("🔄 Enviando approve para MAX_UINT256...");
        const tx = await tokenContract.approve(NEW_EXECUTOR_ADDRESS, MAX_ALLOWANCE);
        
        console.log("🔗 Tx Hash:", tx.hash);
        console.log("⏳ Aguardando confirmação...");
        
        const receipt = await tx.wait();
        console.log("✅ USDC: Aprovação confirmada no bloco", receipt.blockNumber);
        
        // 4. Verificar nova allowance
        const newAllowance = await tokenContract.allowance(signer.address, NEW_EXECUTOR_ADDRESS);
        console.log("📋 Novo allowance:", ethers.formatUnits(newAllowance, decimals), "USDC");
        
    } catch (error) {
        console.error("❌ ERRO na aprovação USDC:");
        console.error("📌 Mensagem:", error.message);
        
        if (error.reason) {
            console.error("📌 Reason:", error.reason);
        }
        
        if (error.code === "CALL_EXCEPTION") {
            console.error("📌 Possível problema: Contrato USDC não acessível");
        }
        
        if (error.code === "INSUFFICIENT_FUNDS") {
            console.error("📌 Saldo de MATIC insuficiente para gas");
        }
    }
    
    console.log("\n🏁 Script de aprovação USDC concluído");
}

// Script alternativo se o principal falhar
async function alternativeApproval() {
    console.log("\n🔄 Tentando método alternativo...");
    
    const [signer] = await ethers.getSigners();
    
    // Método mais direto - sem verificações
    const usdcAddress = "0x2791bca1f2de4661ed88a30c99a7a9449aa84174";
    const erc20Abi = [
        "function approve(address spender, uint256 amount) external returns (bool)"
    ];
    
    try {
        const usdc = new ethers.Contract(usdcAddress, erc20Abi, signer);
        const tx = await usdc.approve(NEW_EXECUTOR_ADDRESS, MAX_ALLOWANCE);
        console.log("🔗 Tx Alternativa:", tx.hash);
        await tx.wait();
        console.log("✅ Aprovação alternativa confirmada!");
    } catch (error) {
        console.error("❌ Método alternativo também falhou:", error.message);
    }
}

main().catch(async (error) => {
    console.error("❌ ERRO FATAL:", error.message);
    
    // Tentar método alternativo em caso de falha
    await alternativeApproval();
    
    process.exit(1);
});