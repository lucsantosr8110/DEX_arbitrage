# Raio-X: Precisão de Cálculo + Latência (Micro-Arbitragem)

**Data:** 2026-07-25  
**Escopo:** `radar.rs`, `arbitrage.rs`, fees/spread/slippage, adapters DEX, flashloan, hot path  
**Objetivo:** cálculos precisos o bastante para capturar micro-arbs; latência ao extremo  
**Método:** leitura de código + cruzamento com fórmulas on-chain (V2/V3/Aave)

---

## Veredito em uma linha

Engine `evaluate_direct` está **alinhado** com quotes fee-inclusive; **radar**, escala de fee V3, `extraData` on-chain e validação de profit no flashloan **quebram** precisão e matam micro-arbs.

---

## 1. Pipeline real (dados → lucro)

```
WS newHeads
  → radar.execute_radar_cycle
      → por DEX saudável: get_prices_multicall (Router/Quoter)
      → prune_non_reciprocal
      → extract_edges          ← só log/audit (NÃO gateia engine)
      → CSV sync no hot path
      → price_tx.send
  → Bot.process_prices
      → ArbitrageEngine.find_arbitrage_opportunities
          → evaluate_direct / triangular
          → force_usdt + recalculate_profitability
          → filtros min_spread / net_profit
      → flashloan: validate → eth_call sim → send
```

**Importante:** edges do radar **não** alimentam a decisão de trade. Divergência radar↔engine polui métricas e filtros rápidos, mas o P&L executável vem do engine.

---

## 2. Fórmulas — estado atual

| Etapa | Fórmula | Onde | Status |
|-------|---------|------|--------|
| Preço | `out_human / in_human` (quote já com fee+impact) | `dex/mod.rs` `calculate_price_from_decimals` | OK |
| Cycle engine | `rate_ab * rate_ba` | `arbitrage.rs` ~1016–1020 | OK (fee já no quote) |
| Cycle radar | `buy*(1-fee)*sell*(1-fee)` | `radar.rs` ~509–510 | **BUG** (fee dupla) |
| Spread | `(cycle_rate - 1) * 100` | ambos | OK se cycle_rate OK |
| Gross USD | `trade_usd * (total_rate - 1)` | `recalculate_profitability` | OK linear |
| Net USD | `gross - gas - flashloan_fee - expected_slippage` | idem | OK conceito; ver bugs fee |
| Slippage min | fee+impact de novo no rate + bps + safety + 2ª slip no flashloan | `arbitrage` + `flashloan` | **OVER-CONSERVADOR** |

### Semântica correta do quote

`getAmountsOut` (V2) e `quoteExactInputSingle` (V3) **já embutem** fee AMM + price impact no notional cotado.  
Aplicar `(1 - fee)` de novo = subtrair fee duas vezes.

---

## 3. Achados críticos (P0)

### P0-1 — Radar deduz fee de novo em preço já fee-inclusive

```505:510:src/dex/radar.rs
let fee_buy = dex_fee(buy_dex, pair);
let fee_sell = dex_fee(sell_dex, rev_pair_str);
let gross_rate = buy_price * sell_price;
let cycle_rate = buy_price * (1.0 - fee_buy) * sell_price * (1.0 - fee_sell);
```

Engine correto:

```1016:1020:src/core/arbitrage.rs
// Rates ... JÁ incluem fee e price impact.
let cycle_rate = rate_ab * rate_ba;
let spread_pct = (cycle_rate - 1.0) * 100.0;
```

**Efeito:** radar reporta cycle_rate ~0.994× do real (V2×V2). Micro-edges (~0.05–0.3%) somem do log/econômicas. Quick-filter pode pular DEXes “frios” por sinal falso.

**Fix:** `cycle_rate = buy_price * sell_price` (igual engine). Manter `gross_rate` só se quiser métrica “antes de custo off-chain” (gas/Aave), não fee AMM.

---

### P0-2 — Uniswap V3 fee dividido por `10_000` em vez de `1_000_000` (~100×)

Fee Uniswap V3: unidades = hundredths of a bip → fração = `fee / 1e6`.

| Fee on-chain | Significado | Código (`/10_000`) | Correto (`/1e6`) |
|--------------|-------------|--------------------|------------------|
| 500 | 0.05% | **5%** | 0.05% |
| 3000 | 0.30% | **30%** | 0.30% |
| 10000 | 1.00% | **100%** | 1.00% |

```372:378:src/dex/radar.rs
"UniswapV3" => {
    if let Some(fee_bps) = super::cached_fee_tier(dex_name, pair) {
        fee_bps as f64 / 10_000.0  // ERRADO
```

Mesmo helper em `arbitrage.rs` `dex_fee` (~1459–1460).  
Teste **codifica o erro**:

```1625:1631:src/core/arbitrage.rs
cache_fee_tier("UniswapV3", pair, 500);
assert_eq!(..., 0.05);   // espera 5%, não 0.05%
cache_fee_tier(..., 10000);
assert_eq!(..., 1.0);    // espera 100%, não 1%
```

**Efeito:** com cache hit + `(1-fee)` no radar, V3 edges viram quase sempre `cycle_rate ≪ 1`. Engine `evaluate_direct` não usa esse helper hoje (bom), mas qualquer path futuro que use `dex_fee` herda o bug.

**Fix:** `fee as f64 / 1_000_000.0`. Renomear parâmetro (`fee_tier` não `fee_bps`). Corrigir testes.

---

### P0-3 — Execução V3 sem fee no `extraData` → default 3000 on-chain

```802:802:src/core/flashloan.rs
extra_data: Bytes::new(),
```

```solidity
// FlashloanExecutor.sol:439
uint24 fee = step.extraData.length > 0 ? abi.decode(step.extraData, (uint24)) : 3000;
```

Radar/adapter podem cotar fee **500** (ou 100); execução força pool **3000**. Sim/exec ≠ quote. Revert ou fill pior.

**Fix:** ABI-encode `uint24` do fee tier cacheado por step; propagar `best_fee` do adapter até `ArbitrageStep` / `AbiSwapStep`.

---

### P0-4 — `validate_profit_after_fees` deduz gas + Aave de novo sobre `net_profit_usd`

Comentário diz evitar dedução dupla; chamada passa `opp.net_profit_usd` e **ainda** subtrai gas + 9 bps:

```286:294:src/core/flashloan.rs
if let Err(e) = self.validate_profit_after_fees(
    opp.net_profit_usd,  // já líquido
    gas_cost,
    ...
```

```117:144:src/core/flashloan.rs
fn calculate_flashloan_fee(...) { amount * 9 / 10000 }  // 9 bps hardcoded
let net_profit_usd = estimated_profit_usd - gas_cost_usd - flashloan_fee_usd;
```

Config: `fee_pct = 0.0005` (5 bps Aave V3). Hardcode = 9 bps.

**Efeito:** micro-arbs com net pequeno (~$0.01–0.10) rejeitados; fee Aave errada em ~4 bps.

**Fix:** validar `net_profit_usd >= min` **ou** recalcular a partir de `gross` com `config.flashloan.fee_pct`. Uma fonte de verdade.

---

## 4. Achados altos (P1)

### P1-1 — Cache de fee tier: chave hex vs símbolo

- `get_price`: cache `hex(tokenA)-hex(tokenB)` (`uniswap_v3.rs` ~446–450)
- Multicall/radar lookup: `"USDC-WETH"` (`~548`)

Miss quase sempre → default 0.3% (ou escala errada do P0-2).

**Fix:** chave canônica única (símbolos ordenados ou addresses checksum lower).

---

### P1-2 — `amount_out_min` re-aplica fee + impact em rate já inclusive

`calculate_expected_output_with_fees` (~1359–1373):  
`amount_after_fee * rate * (1 - impact)` — mas `rate` já veio do quoter.

Depois: `apply_slippage_safe` + **segunda** `apply_slippage` no flashloan (~795).

**Efeito:** mins apertados demais → sim falha em arb que math dizia ok; ou deixa lucro na mesa com floor alto demais para micro-spread.

**Fix para micro-arb:**  
`expected_out ≈ amount_in_human * rate` (sem re-fee), depois só `slippage_bps` + margem pequena. Preferir re-quote on-chain no tamanho real antes do send.

---

### P1-3 — Slippage linear fixo no P&L (`default_price_impact_bps = 25`)

Net sempre desconta 0.25% do notional, independente de profundidade.

Em trade $100 → −$0.25 sempre. Mata edges com spread < ~gas+Aave+0.25%.

**Fix:** impact do próprio quote (comparar quote pequeno vs trade size) ou curva local com reserves; não constante flat.

---

### P1-4 — I/O síncrono de CSV no hot path do radar

`log_price_audit` / `log_edge_audit` (`radar.rs` ~598–618, ~655+) abrem/append arquivo **todo ciclo de bloco**.

**Efeito:** syscall + disk no caminho crítico block→detect.

**Fix:** buffer async / channel para writer dedicado; flag env off em prod low-latency; amostrar (1/N blocos).

---

### P1-5 — `MIN_POOL_LIQUIDITY_USD` e `min_liquidity` config não aplicados

Constante `5000` em radar (~356) **nunca usada**. Config `min_liquidity = "50000"` não gateia adapters. Sem `getReserves` / TVL.

Dust pools → spreads fantasmas → tempo/RPC gasto.

**Fix:** multicall `getReserves` (V2) / `liquidity`+`slot0` (V3); filtrar antes de emitir preço.

---

## 5. Achados médios (P2)

| ID | Achado | Onde | Notas |
|----|--------|------|-------|
| P2-1 | Clone pesado `previous` + `pairs` por task DEX | `radar.rs` ~709–715 | Prefer `Arc` compartilhado |
| P2-2 | V3 multicall 4 fees × par, chunks 10 | `uniswap_v3.rs` | Prefer fee cacheado + 1 call; fallback 4 |
| P2-3 | JoinSet espera DEX mais lento (timeout 30s) | `manager.rs` | Soft deadline curto; partial prices |
| P2-4 | Logging `info!` denso por ciclo | `arbitrage` / `radar` | `debug` / amostragem |
| P2-5 | Flashloan fee 5 bps config vs 9 bps code vs docs | vários | Unificar |
| P2-6 | f64 em todo path; `f64_to_u256` trunca | `utils.rs` | Micro-arb: aritmética inteira U256 |
| P2-7 | Decimals fallback 18 silencioso | `get_token_decimals.rs` | USDT/WBTC quebram preço |
| P2-8 | Triangular pega melhor edge por hop (mix DEX) | `build_price_graph` | OK se steps guardam venue; validar |
| P2-9 | `sanitize_dex_name` → QuickSwap desconhecido | `arbitrage.rs` | Rota errada em pernas sintéticas |
| P2-10 | Coingecko TTL ~2 min no tamanho do quote | `quote_amount_for_usd` | Size errado → impact errado |
| P2-11 | Factory addresses config vs Polygon known | `config.toml` ~540+ | Verificar; V3 tem fallback no código |
| P2-12 | Adapter `swap()` V3 hardcode fee 3000 | `uniswap_v3.rs` ~585 | Perigoso se path usado |
| P2-13 | Quoter V2 address diverge entre `mod.rs` e adapter | adapters | Uma constante |
| P2-14 | `docs/CALC_AUDIT.md` desatualizado | docs | Ainda descreve `(1-fee)` no engine |
| P2-15 | `spread.rs` helper morto | `dex/spread.rs` | Remover ou unificar |
| P2-16 | HighHitRateFilter usa **previous** cycle | `radar.rs` | Pode skip DEX que acabou de divergir |

---

## 6. Constantes — tabela de verdade sugerida

| Item | Atual | Correto / recomendado |
|------|-------|------------------------|
| V2 fee | 0.003 | 0.003 (só se reaplicar; prefer não) |
| V3 fee unit | `/10_000` | `/1_000_000` |
| Aave V3 premium | code 9 bps / config 5 bps | **ler on-chain** `FLASHLOAN_PREMIUM_TOTAL` ou config única |
| Reciprocity | 0.95–1.01 | OK para filtro de lixo |
| `min_spread_percent` | `"0.50"` (config) | Micro-arb: baixar **depois** de fees corretas (ex. 0.05–0.15) senão só filtra |
| `default_price_impact_bps` | 25 | Dinâmico por quote |
| `safety_margin_bps` | 9800 | OK; evitar stack com fee re-aplicada |
| Quote notional | $100 | Calibrar ao size real de execução |
| Multicall timeout | 30s | 200–800ms soft para hot path |

---

## 7. Latência — melhorias priorizadas

Ordem sugerida (ganho / esforço):

1. **Tirar CSV sync do hot path** (P1-4) — ms–dezenas ms por bloco  
2. **Fee tier V3 em cache + 1 quoter call** (P2-2) — corta ~75% payload V3  
3. **`Arc` pairs/prices, menos clone** (P2-1)  
4. **Soft deadline por DEX** (P2-3) — não esperar 30s no pior caso  
5. **Reduzir `info!` por ciclo** (P2-4) — lock/format custa  
6. **Pre-resolvido: addresses + decimals no boot** — zero RPC no path quente  
7. **Encoding calldata pré-montado** para rotas quentes (template + amount)  
8. **Private relay / builder** já parcialmente no config — garantir path sem mempool público  
9. **Local node / WS dedicado** (não Alchemy compartilhado rate-limited)  
10. **Simulação:** `eth_call` no mesmo provider WS/HTTP keep-alive; evitar DNS/TLS novo

Não-ganho: otimizar f64 multiply no cycle_rate — irrelevante vs RPC/I/O.

---

## 8. `radar.rs` — raio-X

**Bom**
- Pares direcionais (sem fabricar `1/price`)
- Reciprocity prune
- Multicall paralelo por DEX
- Disparo por bloco

**Ruim / falha**
- Fee dupla + escala V3 errada (P0-1, P0-2)
- `extract_edges` ≠ economics do engine (métricas mentem)
- CSV no hot path
- Liquidez constante morta
- Quick filter em preço **anterior**

---

## 9. `arbitrage.rs` — raio-X

**Bom**
- `evaluate_direct` fee-inclusive (comentário e fórmula corretos)
- Filtros NaN/Inf, spread max, profit ratio
- `apply_slippage_safe` com clamp de safety margin
- USDT rebuild + filtro por `net_profit_usd`

**Ruim / falha**
- `dex_fee` V3 escala errada (+ teste errado)
- P&L com impact flat 25 bps
- `calculate_expected_output_with_fees` re-deduz fee/impact
- Logging pesado
- Dex placeholder QuickSwap em sanitização
- Path triangular otimista sem re-validar liquidez por hop no size real

---

## 10. Simulação vs realidade (gaps)

| Assunção | Gap |
|----------|-----|
| Quote $100 Coingecko | Size ≠ execução; impact errado |
| Mesmo bloco WS | Próximo bloco / mempool move pool |
| Fee V3 no quote | Exec default 3000 |
| Net no engine | Re-check flashloan destrói margem |
| Sem reserves | Sem size ótimo / depth check |
| f64 | Erro wei em notionals grandes |

Para micro-arb: **re-quote atómicamente** no `amount_in` final imediatamente antes do `eth_call`, no mesmo block number se possível.

---

## 11. Milestone

**Nome:** `M1 — Precision + Latency P0`  
**Branch:** `milestone/m1-precision-latency-p0`  
**Meta:** corrigir bugs que impedem micro-arb + cortar latência óbvia no hot path  
**Done quando:** checklist §12 verde + radar cycle_rate == engine product + fee V3 `/1e6` + `extraData` com fee real + profit check único

| Fase | Escopo | Issues / tasks | Status |
|------|--------|----------------|--------|
| **A — Precisão** | P0 + P1-1/P1-2 | A1–A6 abaixo | ⬜ open |
| **B — Micro-arb** | impact dinâmico, liquidez, thresholds | B1–B4 | ⬜ blocked by A |
| **C — Latência** | CSV off-path, V3 1-call, Arc, soft deadline | C1–C4 | ⬜ pode paralelizar pós-A parcial |

### Fase A — precisão (obrigatório antes de baixar thresholds)

1. **A1** Radar: remover `(1-fee)` do cycle_rate  
2. **A2** `fee / 1_000_000` + testes  
3. **A3** Unificar chave cache fee tier  
4. **A4** Propagar fee V3 em `extraData`  
5. **A5** Flashloan: uma validação de profit; fee = config/on-chain  
6. **A6** `amount_out_min`: não reaplicar fee AMM no rate do quoter  

### Fase B — micro-arb

7. **B1** Impact dinâmico (dual-quote)  
8. **B2** Gate de liquidez real  
9. **B3** Baixar `min_spread` / flat impact só depois de A  
10. **B4** Path U256 onde P&L decide execução  

### Fase C — latência

11. **C1** Audit I/O off hot path  
12. **C2** V3 single-fee + Arc  
13. **C3** Soft deadline / less log  
14. **C4** Metadata warm no boot  

---

## 12. Checklist de verificação pós-fix

- [ ] Unit: V3 fee 500 → 0.0005; 3000 → 0.003; 10000 → 0.01  
- [ ] Unit: radar cycle_rate == `buy*sell` para fixtures fee-inclusive  
- [ ] Integration: step V3 com `extraData` decode = fee cotado  
- [ ] Integration: `validate_profit` não rejeita opp com `net > min` e gas já descontado  
- [ ] Bench: tempo `execute_radar_cycle` p50/p99 com audit off  
- [ ] Paper: comparar net engine vs profit real sim `eth_call` balance delta  

---

## 13. Arquivos tocados na auditoria

| Arquivo | Papel |
|---------|--------|
| `src/dex/radar.rs` | Ingest + edges |
| `src/core/arbitrage.rs` | Detect + P&L + slippage |
| `src/core/flashloan.rs` | Sim/exec + re-check |
| `src/dex/adapters/uniswap_v3.rs` | Quote + fee cache |
| `src/dex/mod.rs` | Price helpers + FEE_TIER_CACHE |
| `contracts/FlashloanExecutor.sol` | Fee default 3000 |
| `config/config.toml` | Thresholds / fees |
| `docs/CALC_AUDIT.md` | **Obsoleto** — não confiar |

---

## Conclusão

Sem Fase A, baixar `min_spread` ou chase micro-arb **aumenta false positives** (radar) e **false negatives** (flashloan/mins).  
Maior leverage: **(1)** cycle_rate radar = produto dos quotes, **(2)** escala fee V3 `/1e6`, **(3)** fee no `extraData`, **(4)** profit check único, **(5)** CSV fora do hot path.
