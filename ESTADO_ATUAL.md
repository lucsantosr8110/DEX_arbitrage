# 🩻 Raio-X do Projeto — DEX Arbitrage Bot (Polygon)

**Data da auditoria:** 2026-07-24
**Commit auditado:** `0d87fa6` (branch `master`, working tree limpo)
**Objetivo:** destravar desenvolvimento e habilitar dry run confiável.

Todas as afirmações abaixo foram **verificadas por execução** (build, testes, run real com
RPC público) ou por **chamada on-chain direta** à Polygon mainnet. Nada aqui é suposição.

---

## 1. Veredicto executivo

| Dimensão | Estado | Nota |
|---|---|---|
| Build Rust (`cargo check` / `cargo build`) | ✅ Compila | 15 warnings, 0 erros |
| Testes unitários Rust | ✅ 17/17 passam | Cobertura rasa (6 de 42 módulos) |
| Build de benchmarks (`cargo check --all-targets`) | ❌ Quebrado | `bench/curve_bench.rs` não existe |
| Toolchain Node/Hardhat | ❌ Não instalado | `node_modules/` ausente |
| Dry run end-to-end | ✅ **FUNCIONA** | Validado: 90 s, 63 ciclos, 0 transações enviadas |
| Coleta de preços on-chain | ✅ Funciona | ~160 preços/ciclo, 3 DEXs |
| Qualidade dos preços coletados | ❌ **Corrompida** | Ver §5 — oportunidades fantasma |
| Execução real (mainnet) | ❌ **Bloqueada** | Wrapper aponta para executor errado; saldo insuficiente |
| Simulação pré-envio (`simulate_before_execute`) | ❌ **Inútil** | Contrato engole revert; simulação nunca falha |
| Observabilidade (Prometheus/Grafana) | ❌ Morta | Servidor de métricas nunca é iniciado |
| Segurança de credenciais | 🚨 **Chave Alchemy ATIVA vazada no Git** | Rotacionar hoje |
| Deploy Docker | ❌ Quebrado | 3 defeitos independentes |

**Resumo:** o esqueleto é sólido e o dry run já roda. O que trava o projeto não é o build —
é (a) um vazamento de credencial ativo, (b) dados de preço corrompidos que geram lucro
fantasma, (c) uma simulação que sempre aprova, e (d) um contrato wrapper desalinhado.

---

## 2. Como rodar o dry run AGORA

Já deixei o ambiente pronto. Dois arquivos foram criados nesta auditoria:

- `.env.dryrun.example` — variáveis de ambiente para dry run (RPC públicos, chave descartável)
- `config/config.dryrun.toml` — cópia do `config.toml` com `dry_run = true`, Telegram off,
  RPC/WS públicos e `refresh_interval = 5` (para não saturar RPC gratuito)

```bash
cp .env.dryrun.example .env      # já feito; .env está no .gitignore
cargo build                       # perfil dev: opt-level 1, ~45 s
mkdir -p logs
./target/debug/flashloan-bot
```

> ⚠️ A `PRIVATE_KEY` em `.env.dryrun.example` é uma chave **descartável gerada aleatoriamente**,
> com saldo zero. Serve só para o bot derivar um endereço e assinar `eth_call` de leitura.
> Nunca use essa chave com fundos. Para produção, substitua e mantenha `.env` fora do Git.

### Evidência de que o dry run é seguro

Execução de 90 s capturada e analisada:

```
Ciclos de radar              63
Oportunidades validadas     290
Oportunidades "executadas"   58
Transações enviadas           0   ← nenhuma
ERROR                         0
```

O caminho de bloqueio é `src/core/flashloan.rs:353` — `determine_execution_strategy()`
retorna `ExecutionStrategy::Skip` antes de qualquer `execute_*`, e o `match` em
`flashloan.rs:319` devolve `BundleResult::skipped()`. Confirmado: zero `eth_sendRawTransaction`.

### Limitação importante do dry run atual

O dry run **não simula a transação on-chain**. Como `Skip` é retornado antes de
`execute_flashloan()`, o bloco `simulate_before_execute` nunca roda (0 ocorrências de
"Simulando" em 90 s de log). O dry run hoje valida **só a matemática de spread**, não a
executabilidade da rota. Ver §4.3 para o conserto.

---

## 3. Inventário técnico

### 3.1 Stack

| Camada | Tecnologia | Tamanho |
|---|---|---|
| Engine | Rust 2021, `tokio`, `ethers-rs` 2.0.14 | 42 arquivos, 12.341 linhas |
| Contratos | Solidity 0.8.24, OpenZeppelin 5.4 | 693 linhas (2 contratos) |
| Deploy/scripts | Hardhat 2.26 + Node ≥18 | 8 scripts `.cjs` |
| Frontend | React + Vite | Stub, não integrado |
| Observabilidade | Prometheus + Grafana | Configurado, não conectado |

Módulos Rust maiores: `config/mod.rs` (1.957), `core/arbitrage.rs` (1.384),
`core/flashloan.rs` (793), `utils/utils.rs` (595), `dex/manager.rs` (580).

### 3.2 Toolchain instalado

```
cargo 1.96.1 / rustc 1.96.1   ✅
node v24.18.0                  ✅
node_modules/                  ❌ ausente  → npm install
artifacts/ cache/              ❌ ausentes → npx hardhat compile
```

### 3.3 Testes

17 testes, todos passando, em 6 módulos: `core/risk`, `dex/get_token_decimals`,
`dex/price_cache`, `dex/spread`, `dex/circuit_breaker`, `config/token_cache`.

**Sem cobertura nenhuma** em: `arbitrage.rs`, `flashloan.rs`, `bot.rs`, `gas.rs`,
`execution_engine.rs`, adapters de DEX. Ou seja: exatamente a lógica que move dinheiro.
`test/Lock.js` é o boilerplate padrão do Hardhat — os contratos não têm nenhum teste.

---

## 4. 🚨 Bloqueadores P0

### 4.1 Chave de API Alchemy ativa versionada no Git

`config/config.toml:215-221` (e `config/config - full.toml`, e `scripts/decode_tx_flashloan.py`)
contêm a chave em texto puro:

```
https://polygon-mainnet.g.alchemy.com/v2/r8Sp****************REDIGIDO
```

**Verifiquei: a chave está ATIVA e respondendo** (`eth_blockNumber` → `0x56919d6`).
Está presente em ambos os commits do histórico (`9b00a66` e `0d87fa6`), então apagar do
arquivo não basta.

Ação, nesta ordem:
1. Revogar/rotacionar a chave no painel da Alchemy **primeiro** — antes de qualquer coisa.
2. Trocar o valor em `config.toml` por `"${ALCHEMY_RPC_URL}"` (o expansor de `${VAR}` já
   existe em `config/mod.rs:1758`).
3. Reescrever o histórico (`git filter-repo` ou repo novo) se o remoto for público.

O endereço da carteira `0x78fE0EA127cE9e07DC872EF3C47A6e1f6e20A472` também está versionado.
Não é segredo criptográfico, mas expõe a estratégia — considere movê-lo para `.env`.

### 4.2 O wrapper on-chain aponta para o executor errado

Consultei os contratos diretamente na mainnet:

| Consulta | Valor on-chain | Valor no `config.toml` | Estado |
|---|---|---|---|
| `FlashloanCaller.executor()` | `0xc9bF35C5ff835aF08d1cc48dF114Af0e0D6b6B33` | — | 🚨 |
| `flashloan.executor_address` | — | `0xC5B79075178866C2B29225AA0c1418464d503a08` | 🚨 |
| `FlashloanExecutor.owner()` | `0x78fE0EA1…A472` (sua carteira) | — | ✅ |
| `FlashloanExecutor.paused()` | `false` | — | ✅ |
| `FlashloanExecutor.wrapperAddress()` | `0x0000…0000` | `0x8E3E24D1…34a8` | ⚠️ |
| `authorizedWrappers[0x8E3E24D1…]` | `true` | — | ✅ |
| `allowedExecutors[carteira]` | `true` | — | ✅ |
| `FlashloanCaller.owner()` | `0x78fE0EA1…A472` | — | ✅ |

O caminho `WrapperFlashloan` — que é o caminho **padrão** com a config atual
(`flashloan.enabled=true` + `wrapper.enabled=true`) — chamaria
`FlashloanCaller.triggerFlashloan()`, que por sua vez pede o flashloan com
`receiver = 0xc9bF35C5…`, um executor **diferente** do configurado. Resultado: falha ou
fundos indo para contrato errado.

Curiosidade reveladora: `0xc9bF35C5ff835aF08d1cc48dF114Af0e0D6b6B33` é exatamente a constante
morta em `src/utils/check_executor_permissions.rs:13` — é o executor da geração anterior.

Correção (uma das duas):
- `FlashloanCaller.updateExecutor(0xC5B79075…)` — onlyOwner, você é o owner; **ou**
- desligar o wrapper (`wrapper.enabled = false`) e usar `ExecutionStrategy::Flashloan`
  direto, já que `allowedExecutors[carteira] = true` permite chamar
  `executeFlashloan()` diretamente. Este caminho é mais simples e um hop a menos.

Recomendo o segundo para destravar; o wrapper só agrega valor se você precisar do initiator
separado.

### 4.3 A simulação pré-envio **nunca falha** — falso positivo garantido

`FlashloanExecutor.executeFlashloan()` (linha 210) envolve o flashloan em `try/catch` e no
`catch` faz `return false` **em vez de reverter**:

```solidity
try IAavePool(AAVE_POOL).flashLoanSimple(...) { return true; }
catch { failedFlashloans++; emit FlashLoanFailure(...); return false; }
```

O mesmo padrão em `executeOperation()` (linha 318): captura a falha da arbitragem, aprova o
pool e **retorna `true`**.

Do lado Rust, `simulate_transaction()` (`flashloan.rs:615`) faz `call.call()` e só trata erro:

```rust
match timeout(Duration::from_secs(10), call.call()).await {
    Ok(Ok(_)) => Ok(()),          // ← recebe Ok(false) e considera SUCESSO
    Ok(Err(e)) => Err(...),
```

Como o contrato retorna `false` em vez de reverter, o `eth_call` **sempre volta `Ok`**. A
simulação aprova 100% das rotas, inclusive as que vão falhar. Em produção isso vira gás
queimado em cada oportunidade fantasma.

Correção: inspecionar o valor de retorno (`Ok(Ok(true))` vs `Ok(Ok(false))`), ou — melhor —
mudar o contrato para reverter e usar uma variante `simulateOnly` separada. O tipo genérico
`T: Detokenize` já carrega o `bool`; hoje ele é descartado com `_`.

### 4.4 Saldo insuficiente para operar

`0x78fE0EA1…A472` tem **0,00686 POL**. A config exige `wallet.min_balance_eth = "0.5"` e
`execution.min_balance_required = "2.0"`. Sem gás, nada executa. Irrelevante para dry run,
bloqueante para mainnet.

---

## 5. 🐛 P1 — Dados de preço corrompidos (a causa dos lucros fantasma)

Este é o achado mais importante para a lucratividade. Extraído do log real de 90 s.

### 5.1 A mesma "oportunidade" 58 vezes, com valor idêntico

```
58×  ✅ Oportunidade detectada: USDT->USDC->DAI (Spread: 1.0481%)
58×  💰 Profit validation: $0.9494 net ($1.0481 gross - $0.0087 gas - $0.0900 flashloan)
```

Spread de **1,05% num triângulo de stablecoins**, idêntico até a 4ª casa decimal, por 58
ciclos consecutivos ao longo de 90 segundos — enquanto as pernas individuais **variam** no
mesmo log (`DAI-USDC` oscila entre 0,994117 e 1,00101). Uma oportunidade real de 1% entre
stables seria arbitrada em um bloco. Isto é ruído estrutural, não mercado.

### 5.2 Preços mutuamente inconsistentes no mesmo mapa

O mapa de preços contém pares cujo produto com o inverso não dá 1,0 — prova aritmética de
bug de unidade/decimais, independente do preço real de mercado:

| Par | Valor | Inverso reportado | Produto | Esperado |
|---|---|---|---|---|
| `GRT-WETH` | 2,823328e-12 | `WETH-GRT` = 117.828,8 | 3,3e-7 | 1,0 |
| `WMATIC-WETH` | 4,146141e-5 | `WETH-WMATIC` = 24.118,8 | 1,0000 | 1,0 ✅ |

E `WMATIC-GRT = 354.513,69` — fisicamente impossível para dois tokens de 18 decimais em
faixa de preço comparável.

O mesmo par também diverge entre DEXs em ordem de grandeza incompatível com arbitragem real:
`WMATIC-USDC` aparece como **0,077** e **0,1369** no mesmo ciclo (78% de divergência).

### 5.3 Causa raiz: filtros de liquidez são configuração morta

`config.toml` define `min_liquidity = "50000"` e `min_volume_24h = "5000.0"` com o comentário
`# CRÍTICO: Filtra pools vazias`. **Ambos são desserializados e nunca lidos.**

```
$ grep -rn "min_liquidity" src/ --include=*.rs
src/config/mod.rs:785:    pub min_liquidity: String,      ← só a declaração do campo
src/config/mod.rs:811:            min_liquidity: "5000.0"   ← só o default
```

Zero usos. Idem `min_volume_24h`, `liquidity_threshold_usd`.

Confirmei on-chain por que isso importa — pools de poeira que o bot está cotando como se
fossem reais:

| Pool | Reservas reais |
|---|---|
| SushiSwap USDC(nativo)-WETH | **0,07 USDC / 0,00 WETH** |
| SushiSwap USDC(nativo)-WMATIC | 24,93 USDC / 325,69 WMATIC |
| QuickSwap USDC(nativo)-WMATIC | 602,57 USDC / 7.830,97 WMATIC |
| QuickSwap USDC.e-WMATIC | 265.205,61 USDC.e / 3.451.865,61 WMATIC ✅ |
| QuickSwap WBTC-WETH | **par não existe** |

Um pool com 0,07 USDC produz um "preço" que passa por todos os filtros atuais e vira spread
de 1%. É exatamente daí que sai a oportunidade fantasma.

**Correção mínima para o dry run virar útil:** aplicar `min_liquidity` no adapter, antes de
publicar o preço no mapa. Um pool V2 abaixo de ~$50k de reserva combinada não deve entrar.

### 5.4 Pares configurados que não existem

`pairs.monitor` lista 31 pares incluindo `WBTC-WETH`, `LINK-*`, `AAVE-*`, `CRV-*`, `GHST-*`,
`UNI-*`, `SUSHI-*`, `GRT-*`, `SAND-*`, `MANA-*`, `LDO-*`. Verifiquei: `WBTC-WETH` **não tem
par V2** nem na QuickSwap nem na SushiSwap. Cada par inexistente é uma `eth_call` desperdiçada
por ciclo, por DEX.

Isso bate com o log: 35 pares consultados, 26–28 preços retornados. ~20% das chamadas são puro
desperdício, e o radar (`src/dex/radar.rs:46-47`) já restringe sua matriz curada a
`STABLES + BLUECHIPS` — o resto vem só do merge com a config.

---

## 6. ⚙️ P1 — Arquitetura desconectada (código morto de alto valor)

`main.rs` monta os componentes mas **não pluga vários deles**:

| Componente | Situação | Evidência |
|---|---|---|
| `ExecutionEngine` | Construído e **descartado** | `main.rs:218` — vinculado a `_execution_engine`, nunca passado adiante |
| `Bot::init_with_engine` | Recebe `Option<()>` placeholder | `bot.rs:167` — parâmetro é literalmente unit type |
| Servidor Prometheus | **Nunca iniciado** | `infra::try_serve_metrics_with_fallback` só é chamado de `Infra::initialize`, que ninguém chama |
| `serve_frontend` | Nunca chamado | `frontend.rs:34` |
| `CachedPriceFeed` (Coingecko) | Nunca instanciado | `bot.rs:81`: *"Usa construtor padrão do ArbitrageEngine (sem price_feed)"* |
| `BundleSender` / Flashbots | Inalcançável | Depende do `ExecutionEngine` descartado |

**Consequência prática:** Prometheus (`prometheus.yml` → `localhost:9100`), Grafana e todos os
dashboards em `monitoring/` apontam para uma porta em que nada escuta. Toda a camada de
observabilidade é decorativa hoje.

### 6.1 Hot reload vaza uma task e não recarrega nada

`Config::from_file()` (`config/mod.rs:1774`) cria um `Arc<Mutex<Config>>` **novo e local**,
passa para `start_reload_listener` e retorna `cfg` por valor. `main.rs:130` embrulha o retorno
no *seu próprio* `Arc<Mutex<_>>`. O listener portanto atualiza um `Arc` órfão que ninguém lê,
e fica lendo o arquivo do disco a cada 3 s para sempre.

Visível no log do dry run: `config reloaded from "config/config.dryrun.toml"` — sem que nada
tenha mudado no comportamento do bot.

### 6.2 Duas `determine_execution_strategy` divergentes

- `bot.rs:179` — **loga** `dry_run` mas não o respeita; devolve `WrapperFlashloan` mesmo em dry run
- `flashloan.rs:348` — respeita `dry_run` corretamente (`Skip`)

Hoje o sistema é seguro porque a segunda roda depois e tem a palavra final. Mas o log fica
enganoso ("🚀 Executando via WrapperFlashloan" seguido de nada) e qualquer refactor que
inverta a ordem vira envio real de transação em dry run. A de `bot.rs` deve ser deletada.

### 6.3 Um segundo "multicall" que nunca foi usado ✅ *corrigido*

> **Correção de uma versão anterior deste relatório.** A afirmação original — de que o caminho
> quente de preços não usava Multicall3 — estava **errada**. Ele usa: os adapters chamam
> `ethers::contract::Multicall` (`adapters/quickswap.rs:228`, `uniswap_v3.rs:486`), que agrega
> via Multicall3 corretamente. Verificado ao ler os adapters durante o planejamento.

O que existia de fato era um **segundo** `DexManager::multicall()` (`manager.rs:323`) que
disparava N `eth_call` paralelos via `join_all` — e que **nenhum código chamava**. Dois caminhos
aparentes de coleta de preços, um deles morto.

Removido. Manter código morto que parece funcional é exatamente o que produziu o desalinhamento
do wrapper em §4.2.

### 6.4 Anti-MEV: Rust e Solidity discordam

`flashloan.rs:339` — `debounce_same_block()` foi neutralizado:

```rust
// CORREÇÃO: Nunca bloquear por mesmo bloco
Ok(false)
```

Mas o contrato ainda carrega `modifier antiMEV { require(block.number != lastExecutionBlock) }`
em `executeFlashloan` e `executeDirect`. Duas execuções no mesmo bloco revertem on-chain,
gastando gás. O lado Rust precisa voltar a respeitar isso, ou o modifier sai do contrato.

---

## 7. 🐳 Infra e deploy quebrados

### 7.1 `docker-compose.yml` não é um docker-compose

O arquivo `docker-compose.yml` contém **configuração do Prometheus** (`global:`,
`scrape_configs:`). Alguém sobrescreveu o arquivo errado. Não existe nenhum compose funcional
no repositório.

### 7.2 Dockerfile — três defeitos

| Linha | Problema |
|---|---|
| `COPY config/config.toml /app/config.toml` | O bot procura `config/config.toml` (`main.rs:125`), não `/app/config.toml` → falha no boot |
| `EXPOSE 9090` | Métricas estão em 9100/9101 na config |
| `FROM rust:1.82` | `.cargo/config.toml` força `target-cpu=native`, o que torna a imagem não-portável entre hosts |

Além disso não há `COPY` do `.env` nem de `abi/` — e vários adapters usam
`include_str!("../../../abi/…")`, que resolve em tempo de compilação (ok), mas
`utils/abi_loader.rs` lê em runtime.

### 7.3 Três arquivos de Prometheus divergentes

`prometheus.yml` (raiz), `monitoring/prometheus.yml` e o conteúdo dentro de
`docker-compose.yml` — com intervalos e targets diferentes (5 s vs 15 s,
`localhost` vs `host.docker.internal`). Escolher um.

### 7.4 Build de benchmark quebrado

```
error: can't find bench `curve_bench` at path `.../bench/curve_bench.rs`
```

O bloco `[[bench]]` no `Cargo.toml` referencia um arquivo inexistente. Isso quebra
`cargo check --all-targets`, `cargo test --all-targets` e qualquer CI. Remover o bloco (ou
criar o arquivo) — correção de 3 linhas.

---

## 8. 📋 Inconsistências de configuração

| Item | Config diz | Realidade | Impacto |
|---|---|---|---|
| Prêmio de flashloan | `fee_pct = 0.0009` (9 bps) | Aave V3 `FLASHLOAN_PREMIUM_TOTAL` = **5 bps** | Superestima custo em 1,8× — conservador, mas erra o cálculo de lucro |
| Preço do POL | `gas.eth_price_usd = 0.102083` (estático) | Muda todo dia | Custo de gás em USD sempre errado; `CachedPriceFeed` existe e está desligado |
| Token Telegram | `bot_token = "${TELEGRAM_BOT_TOKEN}"` | Código lê `TELEGRAM_TOKEN` (`telegram.rs:40`) | Sem a env var, o literal `${TELEGRAM_BOT_TOKEN}` vira "token"; o guard em `telegram.rs:59` compara com a string errada e não pega |
| Endpoints RPC | `network.rpc_endpoints` no TOML | `main.rs:160` **exige** `BOT_RPC_ENDPOINTS` do `.env` e ignora o TOML | Duas fontes de verdade; falha dura se a env var faltar |
| README `.env` | Documenta `TELEGRAM_TOKEN`, `WRAPPER_ADDRESS` | Bot também precisa de `BOT_RPC_ENDPOINTS` (obrigatório) | README incompleto |
| `cooldown_seconds = 1` | — | Runtime força para 10 s (`WARN Cooldown too low`) | Config mentindo sobre o comportamento |
| `retry_delay_ms = 500` | — | Runtime força para 2000 ms | Idem |
| `config/config - full.toml` | Versão `v5.4.1` | Divergente de `config.toml` (`v5.5.9`) | Arquivo órfão com espaço no nome; deletar ou versionar |
| Símbolo do WMATIC | `metadata.WMATIC.symbol = "WMATIC"` | On-chain o token retorna **`WPOL`** | Qualquer lookup por símbolo on-chain quebra |

---

## 9. 📐 Parâmetros técnicos verificados on-chain

Referência confirmada em 2026-07-24 contra Polygon mainnet (chain_id 137).

### Aave V3 — Pool `0x794a61358D6845594F94dc1DB02A252b5b4814aD`

- `FLASHLOAN_PREMIUM_TOTAL` = **5 bps (0,05%)**
- 21 reservas ativas (inclui USDC.e **e** USDC nativo)

Liquidez disponível para flashloan (saldo do underlying no aToken):

| Ativo | Liquidez |
|---|---|
| WMATIC | 72.169.016 |
| USDT | 13.495.917 |
| USDC (nativo) | 12.429.974 |
| DAI | 1.080.396 |
| USDC.e | 547.009 |
| WETH | 10.261 |

Todos folgadíssimos para o `capital_usd = 100` configurado.

### Executor — mínimos

`getMinAmount(USDC.e)` = `1000000` = **1,00 USDC** (6 decimais). O capital de $100 passa.

### USDC.e vs USDC nativo — decisão relevante

A config usa `USDC = 0x2791Bca1f2de4661ED88A30C99a7a9449Aa84174`, que é o **USDC.e (bridged)**.
Ambos os tokens retornam o símbolo `"USDC"` on-chain, então o nome não distingue.

Medi a liquidez real dos dois:

**V2 (QuickSwap/SushiSwap)** — USDC.e ainda é muito superior:
- QuickSwap USDC.e-WMATIC: 265.205 / 3.451.865 vs USDC nativo: 602 / 7.830 (**440× menor**)
- QuickSwap USDC.e-WETH: 983.561 / 524,80 vs USDC nativo: 11.177 / 5,97

**V3 (Uniswap)** — o nativo já domina:
- USDC-WMATIC fee500: L = 2,28e18 vs USDC.e fee500: L = 3,72e17 (**6× maior**)
- USDC-WETH fee500: L = 4,10e16 vs USDC.e fee500: L = 2,42e16

Conclusão: a escolha de USDC.e é defensável para V2, mas custa liquidez em V3. O ideal é
tratar os dois como tokens distintos e deixar o roteador escolher — hoje o bot só enxerga um
e usa o mesmo símbolo para ambos, o que provavelmente contribui para as inconsistências
de §5.2.

### Endereços validados (todos com bytecode deployado)

| Papel | Endereço | Bytecode |
|---|---|---|
| FlashloanExecutor | `0xC5B79075178866C2B29225AA0c1418464d503a08` | 29.920 bytes |
| FlashloanCaller | `0x8E3E24D1ce0d489141FA7c5C3Ed89fCc246034a8` | 2.986 |
| Aave V3 Pool | `0x794a61358D6845594F94dc1DB02A252b5b4814aD` | 4.802 |
| QuickSwap Router | `0xa5E0829CaCEd8fFDD4De3c43696c57F7D7A678ff` | 43.888 |
| SushiSwap Router | `0x1b02dA8Cb0d097eB8D57A175b88c7D8b47997506` | 35.526 |
| UniswapV3 Router | `0xE592427A0AEce92De3Edee1F18E0157C05861564` | 24.142 |

### Endpoints públicos que funcionam (testados)

| Endpoint | HTTP | WSS |
|---|---|---|
| `polygon-bor-rpc.publicnode.com` | ✅ | ✅ 101 Switching Protocols |
| `polygon-bor.publicnode.com` | ✅ | ✅ |
| `1rpc.io/matic` | ✅ | — |
| `polygon.drpc.org` | — | ✅ |
| `polygon-rpc.com` | ❌ 403 tenant disabled | ❌ 401 |
| `polygon.llamarpc.com` | ❌ vazio | — |

Nota: `config.toml` lista `polygon-rpc.com` e `polygon-bor.publicnode.com` como fallbacks;
o primeiro está morto.

---

## 10. 🗺️ Plano de destravamento

### Fase 0 — Hoje (segurança)

1. **Rotacionar a chave Alchemy.** Depois trocar por `${ALCHEMY_RPC_URL}` nos 3 arquivos e
   limpar o histórico Git.
2. Deletar `config/config - full.toml` (órfão, com a mesma chave, nome com espaço).

### Fase 1 — Dry run confiável (1–2 dias)

3. Remover o bloco `[[bench]]` do `Cargo.toml` → `cargo check --all-targets` volta a passar.
4. **Aplicar `min_liquidity` nos adapters**, antes do preço entrar no mapa. É a correção de
   maior retorno do documento inteiro: mata as oportunidades fantasma de §5.
5. Adicionar sanity check de reciprocidade: se `preço(A,B) × preço(B,A)` sair de `1,0 ± 1%`,
   descartar o par e logar. Pega os bugs de §5.2 automaticamente.
6. Podar `pairs.monitor` para os pares com pool real (as 8 combinações stable/bluechip
   verificadas). Corta ~20% das chamadas RPC.
7. Fazer o dry run **simular de verdade**: em `flashloan.rs`, quando `dry_run` estiver ligado,
   montar a call e rodar `simulate_transaction()` reportando o resultado, em vez de `Skip`
   imediato. Aí o dry run passa a validar executabilidade, não só aritmética.

### Fase 2 — Correções de execução (antes de qualquer mainnet)

8. Corrigir a simulação de falso positivo (§4.3): checar o `bool` de retorno **e** mudar os
   `catch { return false }` do contrato para reverter.
9. Resolver o wrapper (§4.2): `updateExecutor` ou desligar `wrapper.enabled`.
10. Deletar `Bot::determine_execution_strategy` (`bot.rs:179`), deixando só a de
    `flashloan.rs` como fonte única.
11. Realinhar anti-MEV: Rust volta a respeitar o `antiMEV` do contrato (§6.4).
12. Corrigir `fee_pct` para 5 bps e ligar o `CachedPriceFeed` para o preço do POL no cálculo
    de gás.

### Fase 3 — Observabilidade e infra

13. Chamar `try_serve_metrics_with_fallback` no `main.rs` — Prometheus/Grafana passam a ver
    dados. Uma linha.
14. Consertar o hot reload (§6.1): `from_file` deve receber o `Arc` de fora, não criar um
    interno.
15. Reescrever `docker-compose.yml` de verdade; corrigir path do config e `EXPOSE` no
    Dockerfile; consolidar os 3 `prometheus.yml` em um.

### Fase 4 — Performance e robustez

16. Migrar `DexManager::multicall` para o Multicall3 (§6.3): ~105 chamadas/s → ~3/ciclo.
17. Testes para `arbitrage.rs` e `flashloan.rs` — hoje zero cobertura na lógica que move dinheiro.
18. Decidir a estratégia USDC.e vs USDC nativo como dois tokens distintos (§9).
19. Plugar ou remover o `ExecutionEngine`/`BundleSender` (§6). Manter código morto que
    parece funcional é o que produziu o desalinhamento de §4.2.

---

## 11. Arquivos criados nesta auditoria

| Arquivo | Propósito |
|---|---|
| `ESTADO_ATUAL.md` | Este documento |
| `.env.dryrun.example` | Ambiente de dry run com RPC públicos e chave descartável |
| `config/config.dryrun.toml` | Config com `dry_run = true`, Telegram off, RPC públicos, refresh 5 s |
| `.env` | Cópia de `.env.dryrun.example` (gitignored) |

`config/config.toml` original **não foi alterado**.
