# 🤖⚡ DEX Arbitrage Bot (Polygon Mainnet)

Este é um robô de arbitragem de alto desempenho, baixa latência e concorrência avançada, projetado especificamente para atuar na rede **Polygon**. 

O projeto adota uma arquitetura híbrida de alto nível:
- **Engine de Decisão (Rust):** Monitora preços em tempo real através de conexões WebSocket resilientes, calcula rotas ótimas de arbitragem triangular e cross-dex em microssegundos e simula a viabilidade financeira antes do envio.
- **Execução On-chain (Solidity):** Contratos inteligentes altamente otimizados realizam a captação de empréstimos rápidos (*Flashloans* via Aave V3) e executam múltiplos swaps sequenciais de forma atômica.

---

## 🚀 Funcionalidades Principais

* **⚡ Algoritmo em Rust:** Processamento concorrente ultra-rápido com `tokio` e `rayon` para identificar distorções de spread.
* **🔄 Rotas de Arbitragem:** Suporte a arbitragem triangular (dentro da mesma DEX) e cross-dex (entre plataformas distintas) limitados a caminhos curtos e eficientes para minimizar o gás.
* **🏦 Integração Multilateral com DEXes:**
  * **Uniswap V2** (e clones como Quickswap V2, Sushiswap V2).
  * **Uniswap V3** (pools de taxas de 0.05%, 0.3%, 1.0%).
* **🔒 Segurança e Resiliência:**
  * **Circuit Breakers:** Desliga a interação com pools temporariamente se desvios ou erros de leitura forem reportados.
  * **Rate Limiters Inteligentes:** Controle e expiração correta de chamadas para nós RPC (como Alchemy e Infura) para evitar bloqueios por concorrência.
  * **Proteção de MEV & Transações Privadas:** Integração para rotear transações através do relay de Flashbots.
  * **Resgate de Nonce & Transações:** Scripts integrados para cancelar e acelerar transações pendentes de forma automática.
* **📈 Observabilidade e Métricas:**
  * Exposição nativa de métricas no padrão do Prometheus.
  * Integração com Grafana (dashboard incluso).
  * Alertas instantâneos no Telegram para lucros, erros e atualizações críticas.

---

## 📂 Estrutura do Repositório

```text
├── abi/                        # ABIs dos contratos das DEXes e Aave
├── ai_models/                  # Modelos auxiliares de ML para estimativa de liquidez
├── config/                     # Arquivos de parametrização (.toml)
├── contracts/                  # Contratos Solidity (FlashloanExecutor e Caller)
├── ignition/                   # Módulos de deploy do Hardhat Ignition
├── monitoring/                 # Provisionamento do Prometheus e Grafana
├── scripts/                    # Scripts administrativos e utilitários (JS/Node)
├── src/                        # Engine do bot escrito em Rust
│   ├── api/                    # Endpoint para monitoramento HTTP
│   ├── core/                   # Lógica de arbitragem, riscos e flashloans
│   ├── dex/                    # Radar, limitadores e adaptadores de DEXes
│   ├── infra/                  # Provedores RPC, métricas e redes
│   └── utils/                  # Notificações do Telegram e utilitários gerais
├── Cargo.toml                  # Dependências do backend em Rust
└── package.json                # Dependências e scripts do Hardhat
```

---

## 🛠️ Pré-requisitos

Antes de iniciar, certifique-se de ter instalado:
* **Rust** (versão estável mais recente, 1.75 ou superior)
* **Node.js** (v18.x ou superior) & **NPM**

---

## ⚙️ Configuração e Execução

### 1. Clonar e Instalar Dependências

```bash
# Instalar dependências do Hardhat e Solidity
npm install
```

### 2. Configurar Variáveis de Ambiente

Crie um arquivo `.env` na raiz do projeto seguindo a estrutura abaixo (o arquivo `.env` está configurado para ser ignorado pelo Git por razões de segurança):

```env
RUST_LOG=info
BOT_JSON_LOGS=false
BOT_NETWORK_NAME=polygon
BOT_NETWORK_CHAIN_ID=137

# RPC: se BOT_RPC_ENDPOINTS estiver ausente, o bot usa [network].rpc_endpoints
# do config.toml (placeholders ${ALCHEMY_RPC_URL} são expandidos automaticamente).
BOT_RPC_ENDPOINTS=https://polygon-mainnet.g.alchemy.com/v2/SUA_API_KEY
ALCHEMY_RPC_URL=https://polygon-mainnet.g.alchemy.com/v2/SUA_API_KEY
ALCHEMY_WS_URL=wss://polygon-mainnet.g.alchemy.com/v2/SUA_API_KEY

PRIVATE_KEY=sua_chave_privada_aqui
EXECUTOR_ADDRESS=0x0000000000000000000000000000000000000000
TELEGRAM_TOKEN=seu_bot_token_aqui
TELEGRAM_CHAT_ID=seu_chat_id_aqui
POLYGONSCAN_API_KEY=sua_api_key_do_scan
```

> **Nota:** `WRAPPER_ADDRESS` foi removido — o wrapper está desabilitado (executor
> errado no FlashloanCaller). O bot usa flashloan direto. Ver `ESTADO_ATUAL.md` §4.2.
>
> **Dry run:** copie `.env.dryrun.example` para `.env` e use
> `CONFIG_PATH=config/config.dryrun.toml` para rodar sem enviar transações.

### 3. Compilar Contratos Solidity

```bash
npm run compile
```

### 4. Executar os Testes Unitários (Rust)

A suíte de testes unitários valida o funcionamento correto de limites de taxa, riscos, buffers de preço e simuladores:

```bash
cargo test
```

### 5. Compilar e Executar o Bot

Para compilar e rodar a engine com todas as otimizações de performance habilitadas:

```bash
cargo run --release
```

---

## ⚠️ Isenção de Responsabilidade

Este projeto foi construído para fins de estudo e experimentação de sistemas distribuídos de alta frequência em redes blockchain. A execução deste robô em redes principais envolve riscos financeiros substanciais devido a volatilidade de mercado, flutuação nas taxas de gás e frontrunning de concorrentes. Os desenvolvedores não se responsabilizam por quaisquer perdas financeiras decorrentes da utilização deste software.
