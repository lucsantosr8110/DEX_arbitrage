const { ethers } = require("hardhat");

async function main() {
    console.log("🔧 Corrigindo autorizações...");
    
    const METAMASK_ADDRESS = "0x78fE0EA127cE9e07DC872EF3C47A6e1f6e20A472";
    const FLASHLOAN_CALLER = "0x8E3E24D1ce0d489141FA7c5C3Ed89fCc246034a8";
    const FLASHLOAN_EXECUTOR = "0xc9bF35C5fF835aF08d1cc48dF114Af0e0D6b6B33";
    
    const [signer] = await ethers.getSigners();
    console.log("👤 Executando como:", signer.address);
    
    if (signer.address.toLowerCase() !== METAMASK_ADDRESS.toLowerCase()) {
        console.log("❌ ERRO: Use a MetaMask correta!");
        return;
    }
    
    const executor = await ethers.getContractAt("FlashloanExecutor", FLASHLOAN_EXECUTOR);
    
    console.log("🔄 Verificando e corrigindo autorizações...");
    
    // 1. Autorizar MetaMask como executor
    const isOwnerExecutor = await executor.allowedExecutors(METAMASK_ADDRESS);
    if (!isOwnerExecutor) {
        console.log("➕ Autorizando MetaMask como executor...");
        const tx1 = await executor.updateExecutor(METAMASK_ADDRESS, true);
        await tx1.wait();
        console.log("✅ MetaMask autorizada como executor");
    }
    
    // 2. Autorizar Caller como wrapper
    const isWrapperAuthorized = await executor.authorizedWrappers(FLASHLOAN_CALLER);
    if (!isWrapperAuthorized) {
        console.log("➕ Autorizando Caller como wrapper...");
        const tx2 = await executor.authorizeWrapper(FLASHLOAN_CALLER, true);
        await tx2.wait();
        console.log("✅ Caller autorizado como wrapper");
    }
    
    console.log("🎯 Todas as autorizações corrigidas!");
}

main().catch(console.error);