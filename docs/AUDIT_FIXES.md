# Auditoria e Correções do Bot de Arbitragem DEX

## v3 — Correções Iniciais (2026-07-24)

### 1. WETH-GRT spread de 2.4 trilhões %
- **Causa**: pools dust (quase vazios) produziam preços garbage
- **Correções**: filtro preço 100k, MAX_SPREAD_PCT=500, GRT removido do allowlist

### 2. Nonce resync a cada 20 segundos
- **Causa**: timer periódico em execution_engine.rs
- **Correção**: removido periodic_resync_task

### 3. Profit filter dedução dupla
- **Causa**: validate_profit_after_fees usava GROSS em vez de NET
- **Correção**: usa opp.net_profit_usd

### 4. is_realistic_price direction-agnóstico
- **Causa**: String::contains() era direction-agnóstico
- **Correção**: match em tuplas (is_stable(token_in), is_stable(token_out))

### 5. evaluate_direct só testava melhor DEX por direção
- **Causa**: pegava melhor rate independentemente
- **Correção**: avalia TODAS as combinações cross-DEX

### 6. Dead code: debug analysis sintético
- **Correção**: bloco removido do main.rs

---

## v4 — Hardening (2026-07-24)

### 1. Validação da matemática cross-DEX com fees (CRÍTICO)
- **Arquivo**: `src/core/arbitrage.rs`
- **Problema**: cálculo anterior usava `rate_ab * rate_ba` sem incluir fees dos pools
- **Correção**: fórmula agora inclui fees de AMBOS os DEXes:
  ```
  cycle_rate = rate_ab × (1 - fee_buy) × rate_ba × (1 - fee_sell)
  ```
- **Fee padrão**: 0.3% para V2 (QuickSwap, SushiSwap) e V3 (UniswapV3 default)
- **Testes unitários**: 3 testes validando:
  - Cenário USDT-WMATIC com fees (mercado eficiente = loss)
  - Cenário hipotético com profit
  - Valores de dex_fee por DEX

### 2. Filtro de liquidez mínima do pool
- **Arquivo**: `src/dex/radar.rs`
- **Constante**: `MIN_POOL_LIQUIDITY_USD = 5000.0`
- **Nota**: filtro preparado mas requer integração com dados de reservas on-chain
  para obter TVL real do pool. Atualmente a allowlist continua como defesa primária.

### 3. Redução do MAX_SPREAD_PCT + zona de auditoria
- **Arquivo**: `src/dex/radar.rs`
- **Mudanças**:
  - `MAX_SPREAD_PCT`: 500.0 → 50.0 (descarta edges absurdos)
  - Zona de auditoria: edges com spread 10-50% são logados em `warn!`
    com par, DEXes e preços para investigação manual
  - Edges > 50% são descartados silenciosamente (garbage residual)

### 4. Proteção do min_profit_absolute em produção
- **Arquivos**: `config/config.toml`, `config/config.dryrun.toml`, `config/config.midtier.toml`
- **Valores**:
  - `config.dryrun.toml`: `min_profit_absolute = 0.05` (dry run detecta oportunidades)
  - `config.toml`: `min_profit_absolute = 0.50` (produção segura)
  - `config.midtier.toml`: `min_profit_absolute = 0.50` (produção segura)

---

## v5 — Radar Edge Semantics (2026-07-24)

### Problema
O radar reportava EDGEs com spread single-direction (ex: USDT-WMATIC 2.42%) que NÃO
eram arbitráveis. A investigação provou com números que:
- Radar detecta: USDT→WMATIC SushiSwap=12.70, UniswapV3=13.01 → spread=2.42%
- Realidade: WMATIC→USDT é ~0.0768 em TODOS os DEXes (flat)
- Cycle_rate máximo: 0.992 (-0.84% loss) — nenhuma combinação cross-DEX é lucrativa

### Correção
- **Arquivo**: `src/dex/radar.rs`
- `extract_edges()` agora valida cycle_rate real antes de emitir EDGE
- Para cada par, verifica se o reverse pair existe no price_map
- Calcula `cycle_rate = buy_price × (1-fee_buy) × sell_price × (1-fee_sell)`
- **EDGE só é emitido quando `cycle_rate > 1.0`** (potencial bruto positivo)
- Spread no EDGE agora reflete `(cycle_rate - 1.0) * 100`, não spread single-direction
- Adicionada função `dex_fee()` (0.3% para V2/V3 — TODO: usar fee tier real)

### Evidência
| Par | Antes (spread single) | Depois (cycle_rate) | Resultado |
|-----|----------------------|---------------------|-----------|
| USDT-WMATIC | 2.42% | 0.992 (-0.84%) | EDGE removido |
| WMATIC-USDT | 2.42% | 0.992 (-0.84%) | EDGE removido |

### Instrumentação removida
- Logs temporários `WMATIC-TRACE`, `WMATIC-SUMMARY`, `WMATIC-PROFIT`, `WMATIC-INPUT`
- Variável `is_wmatic_pair` e `is_wmatic_opp` removidas
- Todo o logging de produção (info!/warn!) mantido

---

## Arquivos Modificados (v5)

| Arquivo | Mudanças |
|---------|----------|
| `src/core/arbitrage.rs` | DEX_FEE_DEFAULT, dex_fee(), cycle_rate com fees, sell_price correto, 3 testes unitários |
| `src/dex/radar.rs` | MIN_POOL_LIQUIDITY_USD, MAX_SPREAD_PCT=50, AUDIT_SPREAD_LOW/HIGH, warn! audit zone |
| `config/config.toml` | min_profit_absolute = 0.50 |
| `config/config.dryrun.toml` | min_profit_absolute = 0.05 (mantido) |
| `config/config.midtier.toml` | min_profit_absolute = 0.50 |

## Conclusão

O bot está correto e identifica que os mercados DEX da Polygon são eficientes.
Não existem oportunidades de arbitragem real com os spreads atuais (2-3%).
O engine corretamente calcula que `rate_ab × rate_ba < 1.0` mesmo sem fees,
e ainda menor com fees de 0.3% em cada DEX.
