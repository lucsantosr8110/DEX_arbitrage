# Fase 2D-C2C — Integração Shadow C2B no bot.rs, sem regressão de latência

Repositório: `/home/lucas/projects/Projeto_Bots/DEX/DEX_arbitrage`
Branch: `phase2d/fresh-executable-discovery`

> Nota: os blocos "Gates" abaixo (`CAMPO=`) são o **relatório de saída** da fase —
> preenchidos ao concluir cada etapa, não parâmetros a decidir agora. Onde um
> valor fixo é exigido pelo requisito (ex.: `CANONICAL_C2B_SHADOW_ENABLED=false`
> por padrão), o valor já está definido no texto.

## Objetivo

Integrar o pipeline canônico C2B diretamente no loop real do bot (`src/core/bot.rs`):

```
legacy hot path — permanece intocado
+
C2B shadow — roda a cada N blocos, em paralelo, fora do runtime crítico
```

Cadeia C2B shadow:

```
CanonicalC2BOpportunitySource
  → RoundEvidence
  → StableOpportunityAggregator (3/3)
  → ExecutableOpportunity
  → RiskManager real
  → StrategySelector real
  → execute_direct / execute_flashloan / execute_wrapper reais
  → dry-run hardcoded (ExecutionMode::CanonicalShadow)
```

Não criar novo binário dedicado — a integração vive em `bot.rs`. O binário
existente `phase2d_c2b_fresh_discovery.rs` vira wrapper fino sobre a lib
extraída (item 3).

## Regras permanentes

```
CANONICAL_C2B_SHADOW_ENABLED=false      # default; liga só via config/env explícito
CANONICAL_C2B_PRIMARY_ENABLED=false     # nunca decide execução real nesta fase
CANONICAL_C2B_BROADCAST_ENABLED=false   # inalcançável nesta fase

MAINNET_WRITE_RPC_CALLS=0
MAINNET_TRANSACTIONS_SENT=0
PRODUCTION_SIGNER_LOADED=false
PRODUCTION_BROADCASTER_INITIALIZED=false
LIVE_EXECUTION_AUTHORIZED=false
```

O caminho legacy mantém exatamente o mesmo comportamento quando C2B shadow
está desligado (`c2b_shadow_disabled_preserves_legacy_behavior`).

## 1. Isolamento do hot path

C2B roda em runtime Tokio dedicado, thread própria:

```rust
std::thread::spawn(|| {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    runtime.block_on(c2b_shadow_service());
});
```

`bot.rs` só: recebe novo bloco → tenta enviar `PinnedAnchor` por canal →
recebe resumo da rodada por outro canal.

Requisitos:
- `C2B_USES_DEDICATED_RUNTIME=true`
- `C2B_USES_DEDICATED_RPC_POOL=true` (pool RPC próprio, não compartilha com legacy/executor)
- `C2B_SHARED_HOT_PATH_LOCKS=0`
- `C2B_DISK_WRITES_IN_LEGACY_LOOP=0`

## 2. Scheduler a cada N blocos

Config:
```
CANONICAL_C2B_SHADOW_ENABLED=false
CANONICAL_C2B_SHADOW_EVERY_N_BLOCKS=<u64>
CANONICAL_C2B_ROUND_TIMEOUT_SECS=<u64>
```

Condição de agendamento — **não** `new_block % n_blocks == 0`:
```rust
new_block >= last_scheduled_anchor.saturating_add(n_blocks)
```

Requisitos:
- bloco e hash pinados (`PinnedAnchor { number, hash }`) antes de agendar
- máximo uma rodada em voo
- canal de anchors com capacidade 1
- nenhum backlog; anchor antigo descartado se canal cheio
- mesmo anchor nunca repetido (`c2b_does_not_repeat_anchor`)
- rodada stale descartada
- reorg invalida a evidência (hash pinado ≠ hash atual do bloco → descarta round)
- erro/timeout do C2B nunca derruba o legacy
- shutdown cancela e aguarda o worker (join/`JoinHandle` com timeout)

Quando ocupado, resultado de agendamento: `C2B_SKIP_REASON=PREVIOUS_ROUND_IN_FLIGHT`.

## 3. Extração do pipeline C2B para a lib

De `src/bin/phase2d_c2b_fresh_discovery.rs`, extrair a rodada de um único
anchor para função de biblioteca (novo módulo, ex. `src/core/c2b_round.rs`):

```rust
pub async fn discover_at(anchor: PinnedAnchor, deps: &C2BRoundDeps) -> Result<RoundEvidence>;
```

Reutiliza os componentes reais já existentes: `ExecutablePriceEdge`,
`ExecutableEdgeGraph`, `StructuralRoute`, `CanonicalExecutionContext`,
`ExecutableRouteMaterializer`, `FreshEconomicsEvaluator`,
`ExecutableCallBuilderRegistry`, `AnvilExecutableReadOnlyVerifier`, fork
preflight, trace validation — mesmos usados hoje pelo bin.

O bin passa a chamar `discover_at` em loop (wrapper fino); nenhuma lógica de
rodada duplicada entre bin e `CanonicalC2BOpportunitySource`.

Uma chamada processa **um** anchor. Não aguarda três blocos.

## 4. `StableOpportunityAggregator`

Novo módulo `src/core/stability.rs`. Extrai do bin (função atual
`stable_candidate_keys`) o predicado puro:

```rust
pub fn is_stable(entries: &[RoundEvidence]) -> bool;
```

Bin e aggregator chamam a mesma função — sem duplicação.

Aggregator mantém janela rolante de três evidências:
```rust
BTreeMap<StabilityKey, VecDeque<RoundEvidence>>
```

```rust
pub struct StabilityKey {
    pub structural_cycle_key: String,
    pub start_token: Address,
    pub amount_in: U256,
    pub execution_profile: ExecutionProfile,
}
```

Estabilidade exige: 3 anchors distintos, 3 block hashes distintos, ordem
crescente, mesma rota, mesmo `amount_in`, mesmo perfil, mesma chain, sem sign
flip, sem revert, eth_call 3/3, preflight 3/3, trace 3/3, context hashes
válidos, zero rejected-registry hit.

**Apenas o aggregator pode emitir `ExecutableOpportunity`.**

## 5. `ExecutableOpportunity`

Novo módulo `src/core/executable_opportunity.rs`:

```rust
pub struct ExecutableOpportunity {
    pub opportunity_id: H256,
    pub structural_cycle_key: String,

    pub route_plan: ExecutableRoutePlan,

    pub anchor_block: u64,
    pub anchor_block_hash: H256,
    pub context_hash: H256,
    pub evidence_hash: H256,

    pub amount_in: U256,
    pub expected_amount_out: U256,

    pub gross_pnl: I256,
    pub gas_estimate: U256,
    pub flashloan_fee: U256,
    pub net_pnl: I256,

    pub net_pnl_usd: Option<f64>,   // relatório apenas

    pub leg_quotes: Vec<LegQuote>,
    pub evidence: ExecutionEvidence,
    pub stability: StabilityRecord,
    pub execution_profile: ExecutionProfile,
}
```

`net_pnl_usd` nunca participa de risk decision, min_profit, sizing ou
execution — só `U256`/`I256`. Nenhuma conversão para `ArbitrageOpportunity`.

## 6. `RiskManager` canônico

```rust
pub fn assess_executable_opportunity(&self, opp: &ExecutableOpportunity) -> RiskAssessment;
```

Reutiliza as políticas/limites comuns do `RiskManager` (não duplica
thresholds). Exige: anchor fresco, hash/context válidos, stability 3/3,
`net_pnl > 0`, gas dentro do limite, slippage dentro do limite, eth_call
pass, preflight pass, trace pass, quotes completas, continuidade da rota,
`rejected_registry_hit = false`.

Aprovação produz:
```rust
pub struct RiskApproval {
    pub min_profit_raw: U256,
    pub max_gas_raw: U256,
    pub max_slippage_bps: u32,
}
```

`min_profit_raw` ≠ lucro esperado integral. Aritmética inteira:
```rust
max(
    configured_absolute_floor,
    expected_net_profit * retention_bps / 10_000,
)
```

## 7. Strategy selector canônico

```rust
pub fn determine_execution_strategy_canonical(
    opportunity: &ExecutableOpportunity,
    approval: &RiskApproval,
    cfg: &Config,
) -> ExecutionStrategy; // Direct | Flashloan | WrapperFlashloan | Skip(reason)
```

Valida capital, lender, token suportado, fee, atomicidade, approvals,
wrap/unwrap, possibilidade de repagamento. Qualquer ausência de capacidade
→ `Skip(reason)`.

## 8. Executor canônico — sem strings nem f64

Proibido no caminho canonical: `extract_and_convert_opp_data`,
`get_token_addr` por símbolo, `map_dex_type` por string, `U256 → f64 → U256`.

Front-end novo:
```
ExecutableRoutePlan → ExecutableCallBuilderRegistry → Vec<ExecutableCall> → AbiSwapStep (conversão estrita)
```

Não reconstrói ABI, selector, router ou fee no executor — usa o builder
registry já validado pela C2B.

Refatoração: extrair núcleo comum de `execute_direct`/`execute_flashloan`/
`execute_wrapper` (simulate → dry gate → send_and_confirm) e criar:

```rust
execute_direct_canonical(...)
execute_flashloan_canonical(...)
execute_wrapper_canonical(...)
```

que chamam o mesmo núcleo produtivo. Tipo explícito de modo:
```rust
pub enum ExecutionMode { LegacyConfigured, CanonicalShadow }
```

Em `CanonicalShadow`: `dry_run=true`, `broadcast_allowed=false`,
`cfg.execution.dry_run` ignorado, `send_and_confirm` inalcançável. Nenhum
bool/env/CLI pode promover `CanonicalShadow` para live. `min_profit_raw` vem
exclusivamente de `RiskApproval`.

## 9. Hard gate dry-run

Reutiliza `execute_direct`/`execute_flashloan`/`execute_wrapper` reais, mas
garante que o path canonical chega ao hard gate com `ExecutionMode::CanonicalShadow`.

Testes obrigatórios: `canonical_shadow_cannot_reach_send_and_confirm`,
`canonical_shadow_cannot_load_production_signer`,
`canonical_shadow_cannot_initialize_broadcaster`,
`canonical_shadow_mode_cannot_be_overridden`. Shadow não pode ter referência
funcional ao broadcaster.

## 10. Integração em `bot.rs`

```
bot.rs
├─ legacy loop existente (price_rx, intocado)
├─ block watcher leve
├─ scheduler C2B
├─ sender de PinnedAnchor
└─ receiver de C2BShadowResult
```

Runtime dedicado processa: anchor → `discover_at` → `RoundEvidence` →
`StableOpportunityAggregator` → `ExecutableOpportunity` → `RiskManager` →
`StrategySelector` → `execute_*_canonical` → hard gate dry-run →
diagnostics. Nunca aguardado dentro do processamento de `price_rx`.

## 11. `DualRunComparator`

Observador sem autoridade decisória — nunca altera decisão/execução legacy.

Compara por: anchor compatível, `start_token: Address`, `amount_in`,
direção, structural route, execution profile.

Classificações: `EXPECTED_MODEL_DIFFERENCE`, `LEGACY_ONLY`, `CANONICAL_ONLY`,
`QUOTE_DIVERGENCE`, `ECONOMICS_DIVERGENCE`, `RISK_DECISION_DIVERGENCE`,
`STRATEGY_DIVERGENCE`, `UNEXPLAINED`.

Persiste em `diagnostics/phase2d_c2c_dualrun_<timestamp>.jsonl` +
`.md`. I/O só no worker/writer dedicado, nunca no hot path.

## 12. Testes obrigatórios

Scheduler/isolamento:
`c2b_shadow_disabled_preserves_legacy_behavior`,
`c2b_runs_after_n_block_distance`, `c2b_does_not_repeat_anchor`,
`c2b_max_one_round_in_flight`, `c2b_queue_never_exceeds_one`,
`c2b_timeout_does_not_stop_legacy`, `c2b_failure_does_not_stop_legacy`,
`c2b_shutdown_cancels_worker`.

Estabilidade:
`three_distinct_anchor_hashes_are_required`,
`reorg_invalidates_round_evidence`,
`stability_key_includes_amount_and_profile`,
`aggregator_only_emits_after_three_stable_rounds`.

Risk/strategy:
`canonical_opportunity_reaches_real_risk_manager`,
`risk_approval_produces_integer_min_profit`,
`approved_opportunity_reaches_strategy_selector`,
`rejected_opportunity_never_reaches_executor`.

Executor:
`canonical_route_never_roundtrips_through_symbols`,
`canonical_calldata_uses_shared_builder_registry`,
`direct_reaches_real_execute_core_in_shadow_mode`,
`flashloan_reaches_real_execute_core_in_shadow_mode`,
`wrapper_reaches_real_execute_core_in_shadow_mode`.

Hard gate:
`canonical_shadow_cannot_reach_send_and_confirm`,
`canonical_shadow_cannot_load_production_signer`,
`canonical_shadow_cannot_initialize_broadcaster`,
`canonical_shadow_mode_cannot_be_overridden`.

Comparator:
`dual_run_comparator_has_no_decision_authority`.

E2E determinístico: bot scheduler → C2B runtime dedicado → `discover_at` →
3 `RoundEvidence` → aggregator → `ExecutableOpportunity` → `RiskManager` →
`StrategySelector` → `execute_*` real → hard gate `CanonicalShadow` →
resultado persistido → zero broadcast.

## 13. Não regressão de latência

Duas sessões equivalentes:
- BASELINE: `CANONICAL_C2B_SHADOW_ENABLED=false`
- EXPERIMENT: `CANONICAL_C2B_SHADOW_ENABLED=true`

Medir: tick→opportunity decision, decision→execute call, p50/p95/p99, CPU,
memória, `price_rx` queue depth, mensagens perdidas, timeouts, opportunities
processed.

Gates:
```
LEGACY_LOOP_P50_REGRESSION_PCT<=2
LEGACY_LOOP_P95_REGRESSION_PCT<=3
LEGACY_LOOP_P99_REGRESSION_PCT<=5

PRICE_RX_DROPPED_MESSAGES=0
PRICE_RX_QUEUE_GROWTH=0
LEGACY_EXECUTION_TIMEOUT_INCREASE=0

C2B_MAX_CONCURRENT_ROUNDS=1
C2B_PENDING_ANCHORS_MAX=1
C2B_USES_DEDICATED_RUNTIME=true
C2B_USES_DEDICATED_RPC_POOL=true
C2B_SHARED_HOT_PATH_LOCKS=0
C2B_DISK_WRITES_IN_LEGACY_LOOP=0
```

Fase falha se p99 ultrapassar o limite, mesmo passando funcionalmente.

## 14. Validações

Diretório isolado: `export CARGO_TARGET_DIR=target/phase2d_c2c`

```
cargo fmt --all -- --check
cargo check --workspace
cargo test --workspace -- --test-threads=1
```

Mais testes direcionados da C2C e Clippy targeted. Dívida legada de Clippy
registrada separadamente de `CLIPPY_NEW_ERRORS_INTRODUCED=0`.

## 15. Smoke operacional

Bot principal com:
```
CANONICAL_C2B_SHADOW_ENABLED=true
CANONICAL_C2B_SHADOW_EVERY_N_BLOCKS=<valor seguro para teste>
CANONICAL_C2B_PRIMARY_ENABLED=false
CANONICAL_C2B_BROADCAST_ENABLED=false
```

Completar ≥3 anchors distintos. Zero oportunidades estáveis é resultado
válido, desde que 3 rodadas completem e os gates arquiteturais/latência
passem.

## 16. Entregáveis

```
diagnostics/phase2d_c2c_report_<timestamp>.md
diagnostics/phase2d_c2c_gates_<timestamp>.txt
diagnostics/phase2d_c2c_shadow_<timestamp>.jsonl
diagnostics/phase2d_c2c_dualrun_<timestamp>.jsonl
diagnostics/phase2d_c2c_latency_baseline_<timestamp>.json
diagnostics/phase2d_c2c_latency_experiment_<timestamp>.json
diagnostics/phase2d_c2c_latency_comparison_<timestamp>.md
```

Sem segredos nos artefatos.

## Gates finais (relatório de saída)

```
PHASE=2D-C2C
VERDICT=

C2B_INTEGRATED_IN_BOT_RS=
C2B_SCHEDULED_EVERY_N_BLOCKS=
C2B_USES_DEDICATED_RUNTIME=
C2B_USES_DEDICATED_RPC_POOL=
C2B_MAX_CONCURRENT_ROUNDS=1

STABILITY_AGGREGATOR_WIRED=
THREE_DISTINCT_ANCHOR_HASHES_REQUIRED=
REORG_EVIDENCE_INVALIDATED=

CANONICAL_RISK_MANAGER_WIRED=
CANONICAL_STRATEGY_SELECTOR_WIRED=
CANONICAL_BUILDERS_SHARED=
CANONICAL_ROUTE_ROUNDTRIPS_THROUGH_SYMBOLS=false

REAL_EXECUTE_METHODS_USED=
CANONICAL_SHADOW_DRY_RUN_HARDCODED=
CANONICAL_SHADOW_DRY_RUN_OVERRIDABLE=false
CANONICAL_BROADCAST_REACHABLE=false

DUAL_RUN_COMPARATOR_WIRED=
DUAL_RUN_COMPARATOR_DECISION_AUTHORITY=false

LEGACY_LOOP_BEHAVIOR_UNCHANGED=
LATENCY_GATES_PASS=

PRODUCTION_SIGNER_LOADED=false
PRODUCTION_BROADCASTER_INITIALIZED=false
MAINNET_WRITE_RPC_CALLS=0
MAINNET_TRANSACTIONS_SENT=0
LIVE_EXECUTION_AUTHORIZED=false

FMT_PASS=
CARGO_CHECK_PASS=
TESTS_PASS=
CLIPPY_NEW_ERRORS_INTRODUCED=0
```

## Commit

Só após todos os gates passarem:
```
feat(2d-c2c): integrate canonical shadow pipeline into bot loop
```
Sem push, sem tag, sem habilitar execução live.
