# Diagnóstico Curve — 2026-07-25

## Q1 — Por que DEXes caiu de 4 para 3?

**Resposta: (b) remoção dinâmica por "0 cotações válidas nesta rodada".** Não é
falha de boot/init. É silenciosa — esse é o bug real.

### Caminho da contagem

1. `src/dex/radar.rs:713-723` — ao final do ciclo, cada DEX só entra em `out` se
   retornou ≥1 cotação:
   ```rust
   Ok(Ok((dex, map))) => {
       if !map.is_empty() { out.insert(dex, map); }
       //  ANTES: else silencioso — DEX sumia sem log
   }
   ```
2. `src/main.rs:103` — `state.dex_count = prices.len();` onde `prices` = `out`.
   Ou seja, `dex_count` = nº de DEXes com ≥1 cotação **neste ciclo**, não nº de
   DEXes configuradas/saudáveis.
3. Rodada B: pair set (config.toml `monitor`, linhas 521-538) **não tem
   stable-stable** (sem USDC-USDT, DAI-USDC, DAI-USDT). Curve só cota stable-stable
   → `get_prices_multicall` retorna vazio → `map.is_empty()` → não entra em `out`
   → `dex_count` 4→3.

### É (a) falha de init? Não.
`src/dex/manager.rs:103-105` constrói `CurveDex` no boot e loga
`"✅ CurveDex inicializado"`. O adapter é saudável; `get_healthy_adapters`
(radar.rs:259) o inclui. A queda é pós-coleta, não pós-init.

### É (c) flag/config? Não.
`config.toml:494` `enabled = true`, `liquidity_threshold_usd = 5000`. A fee
("Curve 0.04%" no rodapé) vem de `fee_tier = 4`. Tudo configurado.

### Bug real: drop silencioso
`radar.rs:716` `if !map.is_empty()` — o `else` não existia. DEX saudável com 0
cotações sumia do `dex_count` sem nenhum WARN/ERROR. Indistinguível de falha de
init/RPC. Compare com `Ok(Err(e)) => warn!` (barulhento) — a falha "válida mas
vazia" era muda.

### Fix aplicada
`radar.rs` agora loga `warn!("🔻 DEX {} retornou 0 cotações — excluída do resumo…")`
no `else`. Drop de DEX é barulhento. 3 testes em `curve.rs` travam o
comportamento stable-only.

---

## Q2 — A cobertura da Curve é só stable-stable?

**Sim. Confirmado no código + RPC.**

### Mapeamento par→pool (curve.rs)
- **Hardcoded, single pool**, não registry/address_provider:
  `CURVE_AAVE_POOL = 0x445FE580eF8d70FF569aB36e80c647af338db351` (am3CRV,
  curve.rs:58). Pool de Aave-wrapped stables: amDAI/amUSDC/amUSDT.
- `pool_tokens` (curve.rs:79-83) = 3 entradas: `(amDAI,18,"DAI"), (amUSDC,6,"USDC"),
  (amUSDT,6,"USDT")`. Só estes símbolos têm índice.
- `get_prices_multicall` (curve.rs:187-192): `pool_index(token_a)` E
  `pool_index(token_b)` ambos Some, senão `continue`. **Só stable-stable
  processa.** Demais pares → `–`.
- **Só stableswap `get_dy`** (ABI curve.rs:33-55). Sem crypto-pools, sem
  `exchange_underlying`, sem tricrypto/twocrypto.
- Quando par sem pool: `continue` **silencioso** (agora `debug!` log,
  curve.rs:191). Honest `–`.

### RPC read-only confirma (executado hoje, sem gas)
```
am3CRV 0x445FE580…  [0]amDAI(18) [1]amUSDC(6) [2]amUSDT(6)   ← já ligado
ATriCrypto3 0x92215849…  [0]am3CRV(18) [1]amWBTC(8) [2]amWETH(18)
ATriCrypto(old) 0x751B1e21… [0]am3CRV [1]amWBTC [2]amWETH
```
Os tricrypto usam **amTokens** (amWBTC/amWETH/am3CRV), não WBTC/WETH/USDC raw.
Não fecham triângulo limpo com o monitor (que usa WETH raw 0x7ceB…, WBTC raw
0x1BFD…). Precisariam de hop wrap/unwrap Aave → não é source de preço 1-hop.

### Por que Rodada A teve Curve e B não
Rodada A: pair set continha stable-stable (provavelmente matriz M3
`curated_matrix_pairs` quando `monitor` vazio, ou config anterior com
USDC-USDT/DAI-USDC/DAI-USDT). Curve cota DAI-USDC, DAI-USDT → entra.
Rodada B: `config.toml monitor` atual = 18 pares, **zero stable-stable** → Curve
0 cotações → cai pra 3.

### Bug latente extra (fix aplicada)
`get_price` (path fallback, curve.rs:138) usa `resolve_am_to_symbol(addr)` que
casava só **amTokens** (amDAI 0x27F8…, amUSDC 0x1a13…). Mas o fallback
(manager.rs:547) passa **endereços raw** (USDC 0x2791…). Mismatch →
`resolve_am_to_symbol` retornava None → `get_price` Curve sempre None, mesmo p/
stable-stable. Multicall (por símbolo) funcionava; o fallback estava morto.
Fix: mapa `RAW_STABLE_TO_SYMBOL` + função livre `stable_symbol_for_address`
(curve.rs). Teste `raw_stable_address_resolves_to_symbol` fixa.

---

## Pools Curve Polygon — vale p/ micro-arb?

| Pool | Addr | Moedas | Fecha triângulo c/ monitor? |
|------|------|--------|-----------------------------|
| am3CRV (stable) | 0x445FE580… | amDAI/amUSDC/amUSDT | **Sim** (USDC-USDT, DAI-USDC, DAI-USDT) — se voltarem ao `monitor` |
| ATriCrypto3 | 0x92215849… | am3CRV/amWBTC/amWETH | **Não limpo** — amTokens, precisa wrap/unwrap |
| ATriCrypto (old) | 0x751B1e21… | am3CRV/amWBTC/amWETH | Idem; TVL baixa (0.37 WBTC) |

**Conclusão honesta:** Curve na Polygon serve stable-stable. Crypto pools usam
amTokens → não são source 1-hop para WETH/WBTC/USDC do monitor. Logo:
- **Não há bug de "pool existe mas código não alcança"** para os pares atuais.
  O `–` é honesto.
- **O bug é a queda silenciosa da contagem** (Q1, fix aplicada) e o `get_price`
  fallback morto (fix aplicada).
- Para Curve voltar a contribuir: **re-adicionar stable-stable ao `monitor`**
  (USDC-USDT, DAI-USDC, DAI-USDT). Mudança de config, não de código. Fecha
  triângulos stable (USDC→USDT→DAI→USDC) vs QuickSwap/Sushi/UniV3.

Ligar tricrypto exigiria: enum/pool Curve genérico + handle de amToken
(deposit/withdraw Aave) + índices. Esforço alto, valor duvidoso (fee 0.04% mas
hop extra come margem). Não recomendado agora.

---

## Patches aplicadas (sem gas, Rust only)

1. `src/dex/radar.rs` — WARN quando DEX saudável retorna 0 cotações (fim do drop
   silencioso).
2. `src/dex/adapters/curve.rs` —
   - `RAW_STABLE_TO_SYMBOL` + `stable_symbol_for_address` (fix `get_price`
     fallback morto por mismatch raw vs am).
   - `debug!` no skip de par não-stable.
   - Funções puras `default_stable_pool_tokens`, `stable_pool_index`,
     `stable_symbol_for_address` (testáveis sem RPC).
   - 3 testes: stable-only index, reject non-stables, raw-address resolution.

## Verificação
`cargo test --lib curve::` → 3 passed. Build limpo (warnings pré-existentes).
Nada on-chain. Sem gas. Segredos só via `.env`.