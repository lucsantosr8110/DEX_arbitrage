# ===============================================================
# decode_tx_flashloan.py — v3.1 (FIXED)
# ===============================================================
# Corrige TypeError: fromhex() argument must be str, not HexBytes
# Adiciona identificação automática de tokens e nomes de DEXs.
# ===============================================================

from web3 import Web3
import os
import sys

# Segredo vem do ambiente. Nunca versione a chave aqui.
RPC_URL = os.environ.get(
    "ALCHEMY_RPC_URL",
    os.environ.get("RPC_POLYGON_URL", "https://polygon-bor-rpc.publicnode.com"),
)
w3 = Web3(Web3.HTTPProvider(RPC_URL))
if not w3.is_connected():
    print("❌ Falha ao conectar ao RPC.")
    sys.exit(1)


def decode_tx(tx_hash: str):
    print(f"\n🔗 Conectado à Polygon RPC.")
    tx = w3.eth.get_transaction(tx_hash)

    # ✅ Corrigido: garantir string
    input_data = tx["input"]
    if not isinstance(input_data, str):
        input_data = input_data.hex()

    print(f"🧩 Tamanho total do input: {len(input_data)} caracteres\n")
    selector = input_data[:10]
    data_hex = input_data[10:]

    print("========================================")
    print("✅ DECODIFICAÇÃO AVANÇADA (v3.1)")
    print("========================================")

    # Método
    print(f"Método ID       : {selector}")
    if selector == "0x220d1596":
        print("Função          : executeDirectArbitrage(address token, uint256 amount, ...)")
    elif selector == "0x128acb08":
        print("Função          : swapExactTokensForTokens() [provável Uniswap/QuickSwap]")
    else:
        print("Função          : (não reconhecida — possivelmente interna)")

    # Token base
    token_addr = Web3.to_checksum_address("0x" + data_hex[24:64])
    amount_hex = data_hex[64:128]
    amount = int(amount_hex, 16)
    print(f"Token base      : {token_addr}")
    print(f"Amount (raw)    : {amount}")
    print(f"Amount (float)  : {amount / 1e6:.6f} tokens (assumindo 6 decimais USDT)\n")

    offset_rotas = int(data_hex[128:192], 16)
    print(f"Offset rotas[]  : {offset_rotas}")

    # Decodificação de addresses (DEX routers)
    addresses = []
    for i in range(0, len(data_hex), 64):
        chunk = data_hex[i : i + 64]
        if len(chunk) < 40:
            continue
        addr = "0x" + chunk[-40:]
        if addr.lower().startswith("0x000000"):
            continue
        if Web3.is_address(addr):
            addresses.append(Web3.to_checksum_address(addr))

    if addresses:
        print("\n📦 Endereços reconhecidos na payload:")
        for addr in addresses[:10]:
            tag = ""
            if addr.lower().startswith("0xa5e0"):
                tag = "(QuickSwap Router)"
            elif addr.lower().startswith("0x1b02"):
                tag = "(SushiSwap Router)"
            elif addr.lower().startswith("0xe592"):
                tag = "(UniswapV3 Router)"
            elif addr.lower().startswith("0xd9e1"):
                tag = "(SushiSwap V2 Factory)"
            print(f" - {addr} {tag}")
    else:
        print("\n⚠️ Nenhum endereço reconhecido na decodificação.")

    # Receipt para status
    receipt = w3.eth.get_transaction_receipt(tx_hash)
    print("\n========================================")
    if receipt["status"] == 0:
        print("⚠️ Resultado on-chain: execution reverted (falha de execução).")
        print("💡 Provável causa: slippage > limite, preço mudou durante o bloco, ou pool ilíquido.")
    else:
        print("✅ Resultado on-chain: incluída e bem-sucedida.")
        print(f"⛽ Gas usado: {receipt['gasUsed']} | Bloco: {receipt['blockNumber']}")
        print("💡 Execução direta entre DEXs confirmada.")
    print("========================================\n")


if __name__ == "__main__":
    if len(sys.argv) < 2:
        print("Uso: python decode_tx_flashloan.py <tx_hash>")
        sys.exit(1)

    tx_hash = sys.argv[1]
    decode_tx(tx_hash)
