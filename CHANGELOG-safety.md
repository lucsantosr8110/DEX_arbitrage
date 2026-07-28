# CHANGELOG-safety — Hardening B1..B10 (branch `fix/execution-safety-r2`)

Diff de defaults alterados (antes → depois) e novos campos com default.
Base: commit `18c151e` (branch `fix/execution-safety`). Sem defaults
inventados: todo novo campo tem `#[serde(default = ...)]` ou `Default`,
e toda mudança de default tem comentário `// SAFETY-EV:` no código.

## Defaults alterados (antes → depois)

| ID  | Campo (config)                        | Antes          | Depois         | Razão (SAFETY-EV)                                                                    |
|-----|---------------------------------------|----------------|----------------|--------------------------------------------------------------------------------------|
| B3  | `mev.public_mempool_min_edge_bps`     | `10`           | `45`           | Mempool público = MEV; 10 bps não cobre sandwich/front-run em Polygon. 45 bps é o piso conservador observado. |
| B4  | `execution.replace_multiplier`        | `Some(1.12)`   | `Some(1.15)`   | 12% falha "replacement underpriced" em congestão; 15% é o teto praticado pelos MEV searchers para RBF confiável. |
| B5  | `execution.adverse_move_bps`          | `0`            | `5`            | ~2s entre quote e inclusão; drift médio de 5 bps/hop em Polygon. 0 continua disponível via config (opt-out explícito). |

## Novos campos com default (antes inexistente → depois)

| ID  | Campo (config)                        | Default        | Razão (SAFETY-EV)                                                                    |
|-----|---------------------------------------|----------------|--------------------------------------------------------------------------------------|
| B2  | `gas.gas_oracle_path`                 | `None`         | Persistência do EWMA por venue é opt-in do operador; `None` = só em memória. Nenhum path inventado. |
| B3  | `mev.allow_public_mempool`            | `false`        | Fail-closed: sem relay privado e `allow=false` → abort `NoPrivateRoute`. Operador opta explicitamente em broadcast público. |
| B4  | `execution.max_replace_attempts`      | `3`            | Teto de re-bumps RBF antes de declarar expirada; evita spam infinito. |
| B4  | `execution.gas_ceiling_gwei`          | `Some(500.0)`  | Teto absoluto de max_fee; excedido → abort `Expired` (não queimar fundo em gas). |
| B6  | `gas.expected_inclusion_blocks`       | `2`            | Janela de projeção do base_fee (12.5%/bloco EIP-1559). Afeta só o custo de gas no EV, não o `max_fee` enviado. |
| B7  | `execution.profit_confirmations`      | `24`           | Confs para promover lucro provisório → final. Polygon ~32 confs p/ finalidade prática; 24 é conservador. |
| B7  | `execution.loss_breaker_threshold`    | `3` (default inalterado; fn exposta `pub(crate)`) | Circuit breaker: 3 perdas realizadas FINAIS consecutivas. Estimated nunca alimenta. |
| B8  | `execution.nonce_stall_secs`          | `45`           | Janela antes do nonce reaper cancelar o nonce preso com no-op self-transfer. 45s > tempo típico de inclusão. |

## Notas

- **B6** não altera `max_fee` enviado — só o custo de gas projetado usado no
  EV. `max_fee` continua vindo do oracle/`max_fee_per_gas` ao vivo.
- **B7** `loss_breaker_threshold` default era 3 e permanece 3; a mudança foi
  expor a fn `default_loss_breaker_threshold` como `pub(crate)` para reuso no
  `ProfitLedger`. Sem mudança de default.
- **B5** `adverse_move_bps = 0` continua válido via config (opt-out explícito);
  o default passa a 5 com `// SAFETY-EV` documentando o drift de ~2s.
- Nenhum secret/key/RPC default foi inventado. Secrets continuam só via `.env`
  gitignored (variáveis de ambiente), nunca argv nem hardcoded.